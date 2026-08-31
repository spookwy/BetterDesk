//! Полный пайплайн этапа 1: захват → энкод → транспорт → декод → окно.
//!
//! **Это главный инструмент этапа 1.** Он отвечает на вопрос, ради
//! которого этап затевался: укладывается ли путь кадра в 25 мс на
//! localhost (docs/roadmap.md, критерий выхода).
//!
//! Транспорт здесь — **заглушка в памяти**, не QUIC и не UDP
//! (docs/roadmap.md, этап 1, пункт 3). Настоящая сеть на этом шаге
//! скрыла бы, где именно теряется время. Но заглушка не «прокидывает
//! байты насквозь»: она честно нумерует кадры, режет их на датаграмы
//! под MTU и собирает обратно — то есть исполняет ровно тот код,
//! который останется после перехода на QUIC.
//!
//! Задержка выводится в заголовок окна и в консоль: `Presented` минус
//! `Captured` по отметкам, которые кадр несёт через весь путь
//! (CLAUDE.md §4.5).
//!
//! Запуск: `cargo run --release -p bd-bench --bin loopback`
//! Дополнительно: `-- --seconds 60 --monitor 1 --link lan`
//!
//! Профили канала для `--link`: `perfect` (по умолчанию), `lan`,
//! `internet`, `mobile` — значения из CLAUDE.md §10.3. Профиль с
//! потерями нужен, чтобы ветка «неполный кадр» в сборщике реально
//! исполнилась: непройденный путь — это не «работает», это
//! «неизвестно» (§0.1, находка 26).

// Проба работает с сырыми указателями D3D11-текстур, поэтому unsafe
// здесь неизбежен. Это инструмент замера, а не часть продукта.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
fn main() {
    println!("BetterDesk работает только на Windows (CLAUDE.md §3.1).");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use bd_bench::any_encoder::AnyEncoder;
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};
    use bd_codec::mediafoundation::D3d11Decoder;
    use bd_codec::{Decoder, DecoderConfig, EncoderConfig, FrameKind};
    use bd_core::frame::FrameSize;
    use bd_core::metrics::{LatencyWindow, Stage};
    use bd_core::time::{now, Epoch};
    use bd_render::windows::VideoWindow;
    use bd_transport::{LoopbackTransport, PayloadKind};
    use std::collections::BTreeMap;
    use std::time::Duration;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let seconds = parse_seconds().unwrap_or(30);

    println!("=== Полный пайплайн: захват → энкод → транспорт → декод → окно ===\n");

    let monitors = enumerate_monitors()?;
    let requested = parse_monitor();
    let target = match requested {
        Some(index) => monitors
            .iter()
            .find(|m| m.index == index)
            .ok_or_else(|| anyhow::anyhow!("монитор {index} не найден"))?,
        None => monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| monitors.first())
            .ok_or_else(|| anyhow::anyhow!("мониторы не найдены"))?,
    };

    // Окно показывает тот же экран, который захватывается, — и своим
    // обновлением создаёт новое изменение экрана. Петля обратной связи
    // гонит DXGI на предельной частоте, и измеренный fps перестаёт
    // что-либо значить. На двух мониторах это лечится захватом
    // соседнего экрана.
    if requested.is_none() && monitors.len() > 1 {
        println!(
            "ВНИМАНИЕ: окно вывода на захватываемом экране создаёт петлю\n\
             обратной связи и завышает fps. Для честного замера:\n\
             --monitor {} (второй экран), окно перетащить туда же.\n",
            monitors
                .iter()
                .find(|m| !m.is_primary)
                .map_or(1, |m| m.index)
        );
    }

    println!(
        "Монитор {}: {} — {}x{}",
        target.index, target.device_name, target.size.width, target.size.height
    );

    // Одна эпоха на весь пайплайн: иначе отметки стадий несравнимы
    // между собой (CLAUDE.md §4.5).
    let epoch = Epoch::new();
    // Захват. Как и у декодера ниже, ошибка объясняется человеку:
    // проба поедет на чужую машину.
    //
    // Клиенту захват своего экрана не нужен — он показывает чужой.
    // Но D3D11-устройство берётся именно отсюда, и на нём живут
    // декодер и окно, поэтому пока капчурер создаётся всем. Это
    // место — кандидат на разделение, когда появится `bd-client`.
    let mut capturer = match DxgiCapturer::new(target.index, epoch) {
        Ok(c) => c,
        Err(err) => {
            println!("\n❌ Захват экрана недоступен: {err}");
            println!();
            println!("Частая причина — гибридная графика (§5.1): DXGI");
            println!("требует, чтобы приложение работало на том же GPU,");
            println!("что и дисплей. Фоллбэк на WGC запланирован.");
            println!();
            println!("Сообщите разработчику эти строки.");
            return Err(err.into());
        }
    };
    // Размер не константа на весь прогон: смена разрешения монитора —
    // самая частая причина `ACCESS_LOST`, и после восстановления
    // дупликация может отдавать другой размер (см. `recover` в
    // bd-capture). Тогда стек кодеков пересоздаётся ниже.
    let mut size = capturer.size();

    // Роль выясняется ДО создания энкодера.
    //
    // # Почему порядок важен
    //
    // Клиент видео не кодирует — он принимает и показывает (§3.1).
    // Но энкодер создавался безусловно, до всякой проверки роли, и
    // на машине без NVIDIA проба падала здесь с «NVENC недоступен»,
    // не дойдя до кода, которому энкодер не нужен вовсе.
    //
    // То есть клиент требовал аппаратного энкодера, чтобы **ничего
    // не кодировать**. Для проверки «работает у всех» это блокер:
    // тестовая машина во Франции — клиент без рабочей видеокарты
    // (§0.1), и до сих пор на ней нельзя было запустить ничего.
    let quic_role = parse_quic_role();
    let needs_encoder = !matches!(quic_role, Some(QuicRole::Client(_)));

    let encoder_config = EncoderConfig::low_latency(size, 60);
    // Битрейт запоминается до того, как конфигурация уедет в энкодер:
    // он нужен итоговому отчёту, а у клиента энкодера нет вовсе.
    let target_bitrate = encoder_config.rate_control.target_bitrate();

    // Энкодер — `Option`: у клиента его нет и быть не должно.
    //
    // Отсутствие энкодера — это не «деградация», а нормальная
    // конфигурация клиента. Поэтому `None` здесь означает «не нужен»,
    // а не «не удалось создать»: вторая ситуация по-прежнему остаётся
    // ошибкой и обрывает запуск у того, кому энкодер нужен.
    let mut encoder = if needs_encoder {
        println!(
            "Энкодер: H.264 {}x{}@60, CBR {} Мбит/с",
            size.width,
            size.height,
            target_bitrate / 1_000_000
        );
        // Бэкенд выбирается под железо: NVENC на NVIDIA, Media
        // Foundation (QuickSync, AMD VCE) на всём остальном.
        // Устройство принадлежит `capturer`, который объявлен раньше
        // энкодера и потому дропается позже.
        let enc = AnyEncoder::new(capturer.device(), encoder_config, epoch)?;
        println!("Бэкенд: {}", enc.backend_name());
        Some(enc)
    } else {
        println!("Энкодер: не нужен (роль клиента — только приём и показ)");
        None
    };

    // Декодер. Ошибка здесь объясняется человеку, а не только
    // разработчику.
    //
    // # Зачем это отдельной веткой
    //
    // Проба поедет на чужую машину, где нет ни отладчика, ни того,
    // кто читал этот код. Голое `?` дало бы строку вида «H.264 MFT не
    // поддерживает D3D11», по которой человек за той машиной не может
    // сделать ничего — он не знает ни что такое MFT, ни что это
    // ожидаемое ограничение, а не поломка сборки.
    //
    // Софт-декодер (`openh264`, §5.5) этот случай закроет
    // по-настоящему; пока его нет, честнее объяснить, что произошло.
    let mut decoder =
        match D3d11Decoder::new(capturer.device(), DecoderConfig::low_latency(size), epoch) {
            Ok(d) => d,
            Err(err) => {
                println!("\n❌ Аппаратный декодер H.264 недоступен: {err}");
                println!();
                println!("Это ограничение машины, а не ошибка сборки.");
                println!("Декодер берётся встроенный в Windows, и он есть");
                println!("почти везде — но требует поддержки D3D11 от GPU.");
                println!();
                println!("Что это значит: на этой машине BetterDesk пока");
                println!("не запустится. Софтверный декодер (openh264)");
                println!("запланирован (CLAUDE.md §5.5), но ещё не сделан.");
                println!();
                println!("Сообщите разработчику эти строки — по ним видно,");
                println!("какой именно случай встретился.");
                return Err(err.into());
            }
        };
    println!("Декодер: H.264 D3D11VA, выход NV12");

    // Транспорт видео: заглушка или настоящий QUIC.
    //
    // Та же эпоха, что у всего пайплайна, иначе отметки Sent/Received
    // окажутся несравнимы с остальными (§4.5).
    //
    // **Заглушка остаётся** после появления QUIC намеренно: она
    // единственный способ измерить пайплайн отдельно от канала.
    // Именно так получена цифра «3.0 мс из 25» — через сеть она
    // утонула бы в сетевом шуме.
    let (link_name, link) = parse_link();
    // Направление видео. Заглушка замкнута сама на себя, а с QUIC
    // стороны разные: хост шлёт, клиент принимает. Обе стороны,
    // шлющие видео, нумеровали бы кадры с нуля, и каждая собирала бы
    // кадр собеседника — так и вышло при первом прогоне.
    use bd_bench::any_transport::{AnyTransport, VideoDirection};

    let (mut transport, direction) = match quic_role {
        Some(QuicRole::Host(bind)) => {
            println!("Транспорт: QUIC, хост на {bind}. Жду клиента (60 с)...");
            let t = AnyTransport::host(bind, Duration::from_secs(60), epoch)?;
            println!("Клиент подключился. Роль: захват и отправка.");
            (t, VideoDirection::SendOnly)
        }
        Some(QuicRole::Client(server)) => {
            println!("Транспорт: QUIC, подключаюсь к {server}...");
            let t = AnyTransport::connect(server, Duration::from_secs(15), epoch)?;
            println!("Соединение установлено. Роль: приём и показ.");
            (t, VideoDirection::ReceiveOnly)
        }
        None => {
            println!(
                "Транспорт: заглушка в памяти, профиль «{link_name}» \
                 (потери {:.1} %, задержка {} мс)",
                link.loss * 100.0,
                link.delay.as_millis()
            );
            (
                AnyTransport::loopback(link, epoch),
                VideoDirection::Loopback,
            )
        }
    };

    // Окно на том же устройстве: копия текстуры между устройствами
    // стоила бы больше, чем весь остальной рендер (CLAUDE.md §4.2.3).
    // Показывается в половинном размере, чтобы окно помещалось на том
    // же экране, который и захватывается.
    let window_size = FrameSize::new(size.width / 2, size.height / 2);
    let mut window = VideoWindow::new(
        capturer.device(),
        "BetterDesk — loopback",
        window_size,
        epoch,
    )?;
    println!("Окно: FLIP_DISCARD, latency 1, Present без VSync\n");

    println!("Замер {seconds} с. Закройте окно, чтобы прервать раньше.");

    // Источник активности на экране. Без него долгий прогон требует,
    // чтобы человек полчаса шевелил окна руками, — именно поэтому
    // критерий «30 минут без утечек» держался открытым дольше всех
    // прочих: не код был не готов, а проверка была дорогой.
    //
    // По умолчанию выключен: агитатор рисует поверх экрана и мешает
    // работать, а короткие замеры и так делаются при живой работе.
    let mut agitator = if parse_flag("--agitate") {
        match Agitator::new((target.origin.0, target.origin.1)) {
            Ok(a) => {
                println!("Агитатор включён: экран будет меняться сам.");
                println!("Сидеть за ПК не нужно — прогон идёт без участия.\n");
                Some(a)
            }
            Err(e) => {
                // Не повод прерывать прогон: агитатор — удобство, а не
                // часть измеряемого пути.
                println!("Агитатор не запустился ({e}), нужна ручная активность.\n");
                None
            }
        }
    } else {
        println!("Нужна активность на экране — подвигайте окна.");
        println!("Либо добавьте --agitate, и проба обеспечит её сама.\n");
        None
    };

    // ── Обратный канал: ввод от клиента к хосту ───────────────────
    //
    // Отдельный экземпляр транспорта, а не тот же самый. Заглушка
    // отдаёт первый собранный кадр независимо от вида, поэтому видео
    // и ввод в одной очереди перепутались бы. В QUIC это будут разные
    // потоки датаграмов — то же разделение, только настоящее.
    //
    // Профиль тот же: ввод едет по тому же каналу, что и видео, и
    // теряется так же. Идеальный обратный канал скрыл бы главную
    // проблему ввода — потерю отпускания клавиши (§5.3).
    let mut input_transport = LoopbackTransport::new(link, epoch);
    let injector = bd_input::windows::InputInjector::new();
    // Трекер на стороне клиента: знает, что зажато, и умеет отпустить
    // всё при потере фокуса.
    let mut client_tracker = bd_input::InputTracker::new();
    // Трекер на стороне хоста: ведёт состояние по приходящим событиям.
    // Отдельный, а не общий: в продукте это разные машины, и
    // состояния расходятся при потерях — именно это и надо видеть.
    let mut host_tracker = bd_input::InputTracker::new();

    let mut raw_input: Vec<bd_render::windows::RawInputMessage> = Vec::new();
    let mut input_sequence = 0u64;
    let mut input_sent = 0u64;
    let mut input_applied = 0u64;
    let mut input_lost = 0u64;
    let mut input_redundant = 0u64;
    let mut input_blocked = 0u64;
    // Последняя известная позиция курсора: подставляется в отпускания
    // кнопок, у которых своей позиции нет.
    let mut last_cursor = bd_core::input::MousePosition::new(0.5, 0.5);

    // ── Курсор: хост → клиент ─────────────────────────────────────
    //
    // Отдельный канал от видео, и это главное решение здесь.
    // Курсор мог бы ехать вмороженным в кадр — но тогда он отставал
    // бы ровно на задержку пайплайна, и человек видел бы, как стрелка
    // тянется за его рукой. Позиция весит 9 байт и успевает прийти
    // раньше картинки (docs/roadmap.md, этап 2).
    //
    // Тот же профиль канала: курсор теряется так же, как всё
    // остальное, и это надо видеть.
    let mut cursor_transport = LoopbackTransport::new(link, epoch);
    let mut cursor_updates = 0u64;
    let mut cursor_shapes_sent = 0u64;
    let mut cursor_shapes_applied = 0u64;
    let mut cursor_rejected = 0u64;

    let inject_input = parse_flag("--inject-input");
    if inject_input {
        println!("Ввод: ВКЛЮЧЁН — окно шлёт события, они применяются к этой же машине.");
        println!("      Это петля: мышь и клавиатура будут работать «сами».");
        println!("      Осторожно — окно должно быть в фокусе, чтобы это было заметно.\n");
    } else {
        println!("Ввод: выключен. Добавьте --inject-input, чтобы проверить (этап 2).\n");
    }

    let started = now();
    let deadline = Duration::from_secs(seconds);
    let frame_timeout = Duration::from_millis(16);

    // Частота кодирования ограничивается той, на которую настроен
    // энкодер: битрейт рассчитан именно на неё. `--fps 0` снимает
    // ограничение — нужно, чтобы показать разницу в замерах.
    let target_fps = parse_arg::<u32>("--fps").unwrap_or(60);
    let mut limiter = bd_core::FrameLimiter::new(target_fps);
    if limiter.is_limited() {
        println!("Ограничение кодирования: {target_fps} кадров/с\n");
    } else {
        println!("Ограничение кодирования СНЯТО (--fps 0)\n");
    }

    let mut total_times = LatencyWindow::new(4000);
    // Разбивка по стадиям: когда итог вырастет, надо знать, какая
    // стадия раздулась (docs/latency-budget.md §4).
    let mut stage_times: BTreeMap<Stage, LatencyWindow> = BTreeMap::new();

    let mut presented = 0u32;
    let mut encoded_frames = 0u32;
    let mut skipped = 0u32;
    let mut recoveries = 0u32;
    // Отдельно от восстановлений: `ACCESS_LOST` чаще всего не меняет
    // геометрию (UAC, перехват), и путать эти два события нельзя —
    // пересоздание стека много дороже пересоздания дупликации.
    let mut resolution_changes = 0u32;
    /// Сколько ошибок захвата подряд терпеть, прежде чем сдаться.
    ///
    /// 600 попыток по 100 мс — это минута. UAC-диалог столько не
    /// живёт, а настоящая поломка за минуту не пройдёт.
    const MAX_CONSECUTIVE_ERRORS: u32 = 600;
    let mut consecutive_errors = 0u32;
    // Оценка смещения часов хоста относительно наших.
    //
    // Нужна только клиенту: метка захвата приезжает в чужой шкале
    // времени, и без поправки задержка вышла бы равной разнице
    // часов — то есть секундам вместо миллисекунд (находка 40).
    let mut clock_sync = bd_core::ClockSync::new();
    // Джиттер-буфер: сглаживает неравномерный приход кадров.
    //
    // Нужен только приёмнику через сеть. У заглушки и у хоста кадры
    // свои, приходят ровно — сглаживать нечего, а буфер добавил бы
    // задержку ни за что.
    let mut jitter_buffer = bd_transport::JitterBuffer::new();
    // Буфер по умолчанию **выключен**, включается `--jitter-buffer`.
    //
    // Это не осторожность, а измерение. Прогон через QUIC на
    // localhost:
    //
    //   без буфера: медиана 21.6 мс, p95 63.6 мс
    //   с буфером:  медиана 34.5 мс, p95 42.8 мс
    //
    // Буфер делает ровно то, ради чего нужен — режет хвост, — но
    // платит за это медианой. На хорошем канале размен невыгоден:
    // 13 мс постоянной задержки дороже редких всплесков. На плохом
    // будет наоборот.
    //
    // Значит, это выбор, а не поведение по умолчанию. Автоматика
    // (включать, когда джиттер превысил порог) — задача этапа 6,
    // где появится контроллер качества; сейчас важнее не встроить
    // необоснованную задержку в замеры этапа 3.
    let use_jitter_buffer =
        direction.receives() && !direction.sends() && parse_flag("--jitter-buffer");
    let mut lost_frames = 0u32;
    // Глубина очереди датаграмов: диагностика расхождения между
    // задержкой профиля и измеренной стадией `network`.
    let mut queue_depth: Vec<u64> = Vec::new();
    let mut max_queue_depth = 0usize;
    // Ключевые кадры: сколько запрошено и сколько энкодер реально
    // выдал. Полный IDR при CBR с VBV в один кадр не помещается в
    // бюджет, и энкодер огрубляет картинку, чтобы уложиться, —
    // это видно как пикселизация всего кадра. Поэтому их частоту
    // надо не предполагать, а считать.
    let mut keyframes_requested = 0u32;
    let mut keyframes_emitted = 0u32;
    // Минимальная пауза между запросами ключевого кадра после
    // потерь. Без неё запрос уходил на каждую потерю, и поток
    // вырождался в сплошные ключевые кадры (подробности у места
    // вызова).
    //
    // # Значение выбрано замером, а не на глаз
    //
    // Прогоны на профиле `lossy` (1 % потерь, 20 мс), 25 с каждый:
    //
    // | пауза  | медиана | p95     | битрейт |
    // |--------|---------|---------|---------|
    // | 250 мс | 330 мс  | 2239 мс | 12.9    |
    // | 150 мс | 150 мс  |  899 мс | 18.4    |
    // | 120 мс | ~140 мс |  ~700мс | ~20     |
    // |  80 мс | 100 мс  |  500 мс | 24.9    |
    //
    // Размен монотонный: короче пауза — быстрее восстановление и
    // дороже трафик. 120 мс взяты как компромисс — восстановление
    // близко к цели, а битрейт не удваивается.
    //
    // **Критерий этапа (p95 ≤ 200 мс) этим не достигается**, и
    // подбором паузы он недостижим в принципе: см. пояснение к
    // вердикту восстановления ниже.
    const KEYFRAME_COOLDOWN: Duration = Duration::from_millis(120);
    let mut last_keyframe_request = started;
    // Сколько запросов подавлено паузой. Нужен, чтобы отличить
    // «ограничение работает» от «потерь не было» — ноль означает
    // разное в этих двух случаях (находка 26).
    let mut keyframes_suppressed = 0u32;
    // Время восстановления после потери — главная метрика критерия
    // этапа 3: «потеря 1 % пакетов не вызывает видимых артефактов
    // дольше 200 мс» (docs/roadmap.md).
    //
    // Меряется от момента, когда потеря обнаружена, до первого
    // ключевого кадра, дошедшего целиком: именно он восстанавливает
    // опору декодера, а до него картинка «сыпется».
    //
    // Без этой цифры критерий проверялся бы глазом, то есть никак:
    // 200 мс — это 12 кадров, и на глаз их от 30 не отличить.
    let mut broken_since: Option<bd_core::time::Instant> = None;
    let mut recovery_times: Vec<Duration> = Vec::new();
    // Размеры кадров по типам: если ключевые в разы крупнее
    // разностных, то при CBR они и есть источник просадки качества.
    let mut key_bytes: Vec<usize> = Vec::new();
    let mut delta_bytes: Vec<usize> = Vec::new();
    let mut closed_early = false;

    // Журнал отправленных кадров: номер, тайминги и копия байтов.
    //
    // Нужен потому, что на канале с задержкой приёмник отдаёт кадр,
    // ушедший несколько итераций назад. Из этого следуют две вещи,
    // и обе легко проглядеть:
    //
    // 1. Сверять целостность надо с оригиналом *того же номера*,
    //    а не с последним закодированным кадром.
    // 2. Задержку надо считать по таймингам доставленного кадра.
    //    Взять тайминги свежего кадра значило бы измерить путь,
    //    которого кадр не проходил, и получить заниженную цифру —
    //    ровно ту ошибку, от которой предостерегает §4.5.
    //
    // Копия байтов — отладочная: продукт кадр в системной памяти
    // не удерживает.
    type SentFrame = (u64, bd_core::metrics::FrameTimings, Vec<u8>);
    let mut sent_log: std::collections::VecDeque<SentFrame> = std::collections::VecDeque::new();
    let mut last_report = started;

    // Строки оверлея. Пересчитываются не каждый кадр: при 60 fps
    // цифры менялись бы быстрее, чем глаз их читает, а форматирование
    // строк в горячем пути — лишняя аллокация на кадр.
    let mut overlay_lines: Vec<String> = Vec::new();
    let mut last_overlay = started;
    let overlay_period = Duration::from_millis(250);

    while now().duration_since(started) < deadline {
        // Насос сообщений прокачивается каждой итерацией, а не по
        // приходу кадра: без этого окно перестаёт отвечать в паузах,
        // когда экран статичен и DXGI отдаёт таймауты.
        if !window.pump_messages() {
            closed_early = true;
            break;
        }

        // Изменение экрана делается до захвата, а не после: иначе
        // первый `AcquireNextFrame` каждой итерации ждал бы таймаут
        // впустую.
        if let Some(a) = agitator.as_mut() {
            a.agitate();
        }

        // ── Ввод: клиент → транспорт → хост ───────────────────────
        //
        // Прокачивается каждую итерацию, а не только когда есть кадр:
        // ввод не должен ждать картинки. Человек, ткнувший мышью,
        // ожидает отклика независимо от того, изменился ли экран.
        {
            raw_input.clear();
            window.drain_input(&mut raw_input);
            let (win_w, win_h) = window.client_size();

            for raw in &raw_input {
                let Some(event) = bd_bench::input_bridge::translate(*raw, win_w, win_h) else {
                    continue;
                };

                // Запоминаем позицию: отпускания кнопок своей не несут.
                if let bd_core::input::InputEvent::MouseMove { position }
                | bd_core::input::InputEvent::MouseButton { position, .. } = event
                {
                    last_cursor = position;
                }

                // Потеря фокуса разворачивается в конкретные
                // отпускания. Слать сам `ReleaseAll` нельзя: он может
                // потеряться, и тогда клавиши останутся зажатыми
                // навсегда. Конкретные отпускания идемпотентны —
                // повторить их безвредно.
                let mut to_send: Vec<bd_core::input::InputEvent> = Vec::new();
                if matches!(event, bd_core::input::InputEvent::ReleaseAll) {
                    to_send.extend(
                        client_tracker
                            .release_events()
                            .map(|e| bd_bench::input_bridge::with_position(e, last_cursor)),
                    );
                    client_tracker.track(&event);
                } else {
                    match client_tracker.track(&event) {
                        bd_input::TrackOutcome::Changed => to_send.push(event),
                        // Автоповтор клавиатуры: хост повторит сам,
                        // гнать это в сеть незачем.
                        bd_input::TrackOutcome::Redundant => input_redundant += 1,
                        bd_input::TrackOutcome::Overflow => {
                            // Зажато предельное число клавиш. Событие
                            // отбрасывается, но это стоит видеть.
                            input_redundant += 1;
                        }
                    }
                }

                for event in to_send {
                    let sequenced = bd_core::input::SequencedInput {
                        sequence: input_sequence,
                        timestamp: epoch.stamp_now(),
                        event,
                    };
                    input_sequence += 1;

                    let mut timings = bd_core::metrics::FrameTimings::default();
                    input_transport.send(
                        PayloadKind::Input,
                        false,
                        &sequenced.encode(),
                        &mut timings,
                    )?;
                    input_sent += 1;
                }
            }

            // Приём и применение. Цикл, а не одна попытка: событий
            // может прийти несколько за итерацию, и копить их —
            // значит копить задержку отклика.
            loop {
                let mut arrival = bd_core::metrics::FrameTimings::default();
                let Some(delivered) = input_transport.receive(&mut arrival)? else {
                    break;
                };
                if delivered.kind != PayloadKind::Input {
                    continue;
                }

                let Some(parsed) = bd_core::input::SequencedInput::parse(&delivered.data) else {
                    // Испорченный пакет не рвёт сессию: он мог прийти
                    // от кого угодно (§8.5).
                    input_lost += 1;
                    continue;
                };

                host_tracker.track(&parsed.event);

                if inject_input {
                    match injector.inject(&parsed.event) {
                        Ok(()) => input_applied += 1,
                        Err(e) if e.is_recoverable() => {
                            // UIPI: окно с большим уровнем целостности
                            // не принимает наш ввод. Не повод рвать
                            // сессию — пользователь переключит окно.
                            input_blocked += 1;
                        }
                        Err(e) => {
                            println!("Ошибка инжекта: {e}");
                            break;
                        }
                    }
                } else {
                    input_applied += 1;
                }
            }
        }

        // ── Курсор: хост → транспорт → клиент ─────────────────────
        //
        // Забирается ДО захвата кадра и прокачивается каждую итерацию:
        // курсор меняется чаще, чем картинка, и именно на статичном
        // экране (находка 7). Ждать кадра значило бы терять движение
        // мыши там, где оно единственное, что происходит.
        {
            if let Some(state) = capturer.take_cursor() {
                // Форма едет первой: позиция ссылается на неё номером,
                // и клиент, получивший позицию раньше формы, рисовать
                // не сможет. Обратный порядок стоил бы одного кадра
                // с невидимым курсором на каждой смене формы.
                if let Some(shape) = state.new_shape {
                    let mut timings = bd_core::metrics::FrameTimings::default();
                    cursor_transport.send(
                        PayloadKind::Control,
                        false,
                        &shape.encode(),
                        &mut timings,
                    )?;
                    cursor_shapes_sent += 1;
                }

                // Вид `Cursor`, а не `Video`: у пробы курсор всё ещё
                // едет своим экземпляром транспорта, и вид здесь ни на
                // что не влияет. Но продукт свёл каналы в один, где вид
                // стал адресом, — и расхождение между пробой и
                // продуктом в том, что именно уходит в провод, рано или
                // поздно стоило бы отладки на ровном месте.
                let mut timings = bd_core::metrics::FrameTimings::default();
                cursor_transport.send(
                    PayloadKind::Cursor,
                    false,
                    &state.position.encode(),
                    &mut timings,
                )?;
                cursor_updates += 1;
            }

            // Приём: цикл, а не одна попытка. Позиций может прийти
            // несколько за итерацию, и показывать надо последнюю —
            // промежуточные уже устарели.
            loop {
                let mut arrival = bd_core::metrics::FrameTimings::default();
                let Some(delivered) = cursor_transport.receive(&mut arrival)? else {
                    break;
                };

                match delivered.kind {
                    // Форма курсора.
                    PayloadKind::Control => {
                        match bd_core::cursor::CursorShape::parse(&delivered.data) {
                            Some(shape) => match window.set_cursor_shape(&shape) {
                                Ok(()) => cursor_shapes_applied += 1,
                                Err(e) => {
                                    // Без курсора работать можно, без
                                    // картинки — нет. Сессию не рвём.
                                    tracing::debug!(?e, "форма курсора не загрузилась");
                                    cursor_rejected += 1;
                                }
                            },
                            None => cursor_rejected += 1,
                        }
                    }
                    // Позиция курсора.
                    _ => match bd_core::cursor::CursorPosition::parse(&delivered.data) {
                        Some(position) => window.set_cursor_position(&position),
                        None => cursor_rejected += 1,
                    },
                }
            }
        }

        // Таймаут захвата одинаков для обеих ролей, и это проверено
        // измерением, а не рассуждением.
        //
        // Гипотеза была такой: приёмнику захват не нужен, поэтому
        // нулевой таймаут даст ему забирать кадры сразу по приходе,
        // и стадия `network` (20.7 мс на localhost) сократится.
        // **Прогон опроверг:** стало 33.5 мс вместо 20.7. Цикл без
        // ожидания крутится на полной скорости и вытесняет поток
        // QUIC, которому и надо-то доставить датаграмы.
        //
        // Оставшиеся ~20 мс — цена того, что приём привязан к такту
        // цикла (родственно находке 34). Лечится не таймаутом, а
        // разделением на потоки: приём и показ должны идти своим
        // темпом, а не в ритме захвата. Это работа для `bd-client`,
        // а не для пробы.
        let frame = match capturer.next_frame(frame_timeout) {
            Ok(CaptureOutcome::Frame(f)) => f,
            Ok(CaptureOutcome::Timeout) => {
                // Экран не изменился — нового кадра нет. Но окно
                // обязано жить: без перерисовки оно выглядит
                // зависшим и «отвисает» только когда пользователь
                // его подвинет (движение меняет экран → DXGI отдаёт
                // кадр). Именно так дефект и проявлялся.
                //
                // Заодно это единственный способ показать движение
                // курсора на статичной картинке: он меняется чаще,
                // чем содержимое экрана.
                //
                // Перерисовка не идёт через транспорт и не считается
                // в задержке: это тот же кадр, а не новый.
                window.redraw_last_frame()?;
                continue;
            }
            Err(err) if err.is_recoverable() => {
                recoveries += 1;

                // Восстановление может не удаться с первой попытки, и
                // это НЕ повод завершать работу. Пока на экране
                // UAC-диалог или экран блокировки, рабочий стол
                // защищён, и `DuplicateOutput` будет отвечать отказом
                // столько, сколько пользователь смотрит на диалог.
                //
                // Раньше здесь стоял `recover()?` — и первый же UAC
                // ронял процесс. Ровно тот сценарий, который критерий
                // этапа 1 требует пережить (docs/roadmap.md).
                //
                // Ожидание в горячем пути допустимо только здесь:
                // кадров всё равно нет, захват уже остановлен не нами.
                let mut waited = Duration::ZERO;
                let backoff = Duration::from_millis(100);
                let give_up_after = Duration::from_secs(60);

                loop {
                    // Окно должно оставаться отзывчивым, пока мы ждём:
                    // иначе Windows пометит его как зависшее, а
                    // пользователь не сможет его закрыть.
                    if !window.pump_messages() {
                        closed_early = true;
                        break;
                    }

                    // Любая ошибка восстановления — повод подождать,
                    // а не выйти.
                    //
                    // Раньше здесь была ветка «сломался необратимо»
                    // для нераспознанных кодов, и она закрывала окно
                    // после UAC: secure desktop возвращает не только
                    // `E_ACCESSDENIED`, но и другие коды, которые
                    // классификация не знает. Пользователь сообщил,
                    // что окно закрывается — это была она.
                    //
                    // Отличать «временно нельзя» от «сломано навсегда»
                    // по коду ошибки ненадёжно: список кодов secure
                    // desktop нигде не документирован полностью.
                    // Надёжнее время: то, что не восстановилось за
                    // минуту, действительно сломано, а UAC-диалог
                    // столько не живёт.
                    match capturer.recover() {
                        Ok(()) => break,
                        Err(e) => {
                            if waited >= give_up_after {
                                println!(
                                    "Захват не восстановился за {} с: {e}",
                                    give_up_after.as_secs()
                                );
                                closed_early = true;
                                break;
                            }
                            if waited.is_zero() {
                                println!("Захват недоступен ({e}) — жду");
                            }
                            std::thread::sleep(backoff);
                            waited += backoff;
                        }
                    }
                }

                if closed_early {
                    break;
                }

                if !waited.is_zero() {
                    println!("Захват восстановлен спустя {:.1} с", waited.as_secs_f64());
                    // Декодер потерял опорный кадр, пока захвата не
                    // было: без ключевого он покажет мусор (§5.3).
                    keyframes_requested += 1;
                    if let Some(e) = encoder.as_mut() {
                        e.request_keyframe();
                    }
                    sent_log.clear();
                }

                // Дупликация восстановлена — но если разрешение
                // сменилось, энкодер и декодер настроены на прежнее.
                // Молча продолжать нельзя: NVENC не меняет размер
                // кадра на лету (реконфигурация покрывает битрейт,
                // но не геометрию), а декодер ждёт SPS прежней
                // геометрии. Без пересоздания это не отказ, а поток
                // мусора — то есть худший вид поломки.
                let fresh = capturer.size();
                if fresh != size {
                    println!(
                        "Разрешение изменилось {}x{} → {}x{}, пересоздаю стек кодеков",
                        size.width, size.height, fresh.width, fresh.height
                    );
                    size = fresh;
                    resolution_changes += 1;

                    // Порядок важен: старый энкодер отпускается до
                    // создания нового. Он держит регистрации текстур
                    // на устройстве захвата, и два энкодера на одном
                    // устройстве — лишний расход сессий NVENC,
                    // которых на потребительских картах немного.
                    // Пересоздаётся только тот энкодер, который был.
                    // У клиента его нет, и создавать здесь означало бы
                    // потребовать NVENC ровно там, где мы только что
                    // договорились без него обходиться.
                    if encoder.is_some() {
                        drop(encoder);
                        // Устройство принадлежит `capturer` и переживает
                        // энкодер — `recover` устройство намеренно не
                        // пересоздаёт (см. bd-capture).
                        encoder = Some(AnyEncoder::new(
                            capturer.device(),
                            EncoderConfig::low_latency(size, target_fps.max(1)),
                            epoch,
                        )?);
                    }

                    decoder = D3d11Decoder::new(
                        capturer.device(),
                        DecoderConfig::low_latency(size),
                        epoch,
                    )?;

                    // Кадры в пути относятся к прежней геометрии:
                    // сверять их не с чем, а декодер их уже не примет.
                    sent_log.clear();
                }
                continue;
            }
            Err(err) => {
                // Нераспознанная ошибка захвата — тоже повод
                // попробовать восстановиться, а не выйти.
                //
                // Это вторая половина того же дефекта, из-за которого
                // окно закрывалось после UAC. Классификация ошибок
                // (`is_recoverable`) знает про `ACCESS_LOST` и
                // `E_ACCESSDENIED`, но secure desktop возвращает и
                // другие коды. Попадая сюда, прогон завершался.
                //
                // Теперь неизвестная ошибка ведёт себя как
                // восстановимая: следующая итерация уйдёт в цикл
                // ожидания выше, а тот сдастся по времени, если
                // поломка настоящая.
                consecutive_errors += 1;
                if consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                    println!("Захват выдаёт ошибки подряд ({consecutive_errors}): {err}");
                    break;
                }
                if consecutive_errors == 1 {
                    println!("Ошибка захвата ({err}) — пробую восстановиться");
                }
                if let Err(e) = capturer.recover() {
                    tracing::debug!(?e, "восстановление не удалось, повтор");
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        // Кадр получен — счётчик ошибок сбрасывается: важна серия
        // подряд, а не общее число за прогон.
        consecutive_errors = 0;

        // Кадр, где изменился только курсор, кодировать незачем (§5.1).
        if !frame.info().content_changed {
            skipped += 1;
            // Кадр не кодируется, но окно перерисовывается: сдвинулся
            // курсор, и его новая позиция должна попасть на экран.
            window.redraw_last_frame()?;
            continue;
        }

        // Ограничение частоты кодирования. DXGI отдаёт кадр на каждое
        // изменение экрана — при активной работе это 150–250 кадров
        // в секунду. Кодировать их все нельзя: при CBR битрейт делится
        // на фактическое число кадров, и каждому достаётся втрое
        // меньше бит, чем заложено. Именно это выглядит как
        // периодическая пикселизация (docs/measurements.md).
        // Ограничитель касается только тех, кто кодирует. Приёмник
        // ничего не кодирует — его дело показывать присланное, и
        // пропускать ради лимита две трети итераций значит держать
        // готовые кадры в буфере, пока цикл занят чужой работой.
        //
        // Это давало фактическое удержание 53 мс при целевом 21 мс
        // и утраивало общую задержку.
        if direction.sends() && !limiter.should_encode_now() {
            // Кадр пропущен ограничителем частоты, но показать
            // последний всё равно надо: иначе окно обновлялось бы
            // только на тех кадрах, что прошли лимит, и курсор
            // двигался бы рывками.
            window.redraw_last_frame()?;
            continue;
        }

        // Кодирование — только у того, кто отправляет.
        //
        // У клиента энкодера нет вовсе (машина может быть без
        // NVIDIA), поэтому здесь строится пустая оболочка: дальше по
        // циклу в неё кладутся байты **принятого** кадра, и весь
        // остальной путь — декод, показ, тайминги — работает без
        // изменений.
        //
        // Захваченный кадр клиенту при этом не нужен: он показывает
        // чужой экран, а не свой. Захват у него всё равно идёт —
        // ради размера кадра и ритма цикла.
        let mut encoded = match encoder.as_mut() {
            Some(enc) => {
                let Some(encoded) = enc.encode(frame.texture(), *frame.info())? else {
                    continue;
                };
                encoded_frames += 1;
                encoded
            }
            None => bd_codec::EncodedFrame {
                kind: FrameKind::Delta,
                data: Vec::new(),
                info: *frame.info(),
            },
        };

        // Кадр уходит в транспорт и приходит обратно уже собранным из
        // датаграмов. Тайминги едут в `encoded.info`: транспорт ставит
        // в них Sent и Received, а сам байты не разбирает (§4.2.4).
        let keyframe = matches!(encoded.kind, FrameKind::Key);
        if keyframe {
            keyframes_emitted += 1;
            key_bytes.push(encoded.data.len());
        } else {
            delta_bytes.push(encoded.data.len());
        }
        // Клиент видео не отправляет: его дело — принимать и
        // показывать. Отправка обеими сторонами приводила к тому,
        // что каждая собирала кадр собеседника.
        if direction.sends() {
            let sequence = transport.send(
                PayloadKind::Video,
                keyframe,
                &encoded.data,
                &mut encoded.info.timings,
            )?;
            sent_log.push_back((sequence, encoded.info.timings, encoded.data.clone()));
        }
        // Журнал не должен расти без предела: кадры, чей номер приёмник
        // уже никогда не выдаст, иначе копились бы вместе со своими
        // байтами. 256 кадров при 60 fps — это 4 секунды, много больше
        // любой мыслимой задержки канала.
        while sent_log.len() > 256 {
            sent_log.pop_front();
        }

        // Отметку Received транспорт ставит в отдельные тайминги: они
        // тут же будут заменены на тайминги доставленного кадра, а
        // тайминги свежего кадра портить нельзя — он ещё в пути.
        // Глубина очереди датаграмов. Если она растёт, значит приёмник
        // опрашивается реже, чем отправитель шлёт, и стадия `network`
        // измеряет не задержку канала, а ожидание своей очереди.
        // Без этого числа расхождение между заявленным профилем и
        // измеренной задержкой невозможно объяснить.
        let depth = transport.in_flight();
        queue_depth.push(depth as u64);
        if depth > max_queue_depth {
            max_queue_depth = depth;
        }

        // Хост в режиме QUIC видео не принимает: его окно показывает
        // собственный захват, чтобы было видно, что он передаёт.
        // Ждать кадров от клиента бессмысленно — тот их не шлёт.
        if !direction.receives() {
            window.redraw_last_frame()?;
            continue;
        }

        let mut arrival = bd_core::metrics::FrameTimings::default();

        // Приём через джиттер-буфер (только у приёмника: у заглушки
        // и у хоста сглаживать нечего — там кадры свои).
        //
        // Буфер придерживает ранние кадры ровно на измеренный разброс
        // задержек. На идеальном канале удержание нулевое, и он
        // прозрачен, — иначе мы платили бы задержкой за услугу,
        // которая не нужна.
        let delivered = if use_jitter_buffer {
            // Забрать всё пришедшее: буфер сам решит, когда выдавать.
            while let Some(frame) = transport.receive(&mut arrival)? {
                let delay = if frame.captured_at_micros != 0 {
                    clock_sync.observe(frame.captured_at_micros, epoch.stamp_now())
                } else {
                    0
                };
                jitter_buffer.push(frame, delay, epoch.stamp_now());
            }

            match jitter_buffer.pop(epoch.stamp_now()) {
                Some(frame) => {
                    // Отметка `Received` ставится при выдаче ИЗ
                    // БУФЕРА, а не при приёме из транспорта.
                    //
                    // Иначе удержание попадает в следующую стадию:
                    // первый прогон показал `decode` 25.6 мс вместо
                    // 0.04 — ровно на величину буфера. Цена буфера
                    // должна быть видна там, где она возникает
                    // (тот же урок, что находка 34).
                    arrival.mark(Stage::Received, epoch.stamp_now());
                    frame
                }
                None => {
                    // Кадра к показу нет: либо буфер пуст, либо время
                    // ближайшего ещё не пришло. Окно при этом должно
                    // жить (находка 37).
                    window.redraw_last_frame()?;
                    continue;
                }
            }
        } else {
            let Some(frame) = transport.receive(&mut arrival)? else {
                // Кадр ещё в пути (канал с задержкой) либо не собрался
                // из-за потерь. Ждать нельзя — устаревший кадр не нужен
                // (CLAUDE.md §5.3), поэтому просто идём за следующим.
                // Потери посчитаются ниже, когда приёмник проедет номер.
                continue;
            };
            frame
        };

        // Пришедший кадр — не обязательно тот, что отправлен в этой же
        // итерации: при задержке канала приёмник отдаёт кадр, ушедший
        // несколько тактов назад. Поэтому сверять байты можно только
        // с *отправленной копией того же номера*, а не с последней
        // закодированной. Отсюда журнал отправленных кадров.
        //
        // Сверка нужна: испорченный при сборке поток H.264 декодер
        // чаще всего проглотит, выдав мутную картинку вместо ошибки
        // (тот же урок, что находка 21).
        let mut lost_now = 0u32;
        while let Some((seq, _, _)) = sent_log.front() {
            if *seq < delivered.sequence {
                // Кадр не дошёл целиком — его номер проехали.
                // Пропускаем его и идём дальше: ждать нельзя, а
                // переспрашивать нечего (§5.3).
                lost_frames += 1;
                lost_now += 1;
                sent_log.pop_front();
            } else {
                break;
            }
        }

        // Восстановление после потерь: ключевой кадр, но НЕ на каждую
        // потерю.
        //
        // # Что было измерено
        //
        // Раньше `request_keyframe` вызывался на каждый потерянный
        // кадр. При 1 % потерь датаграмов это дало:
        //
        //   запрошено 459 ключевых, выдано 295 — 707 в минуту
        //   битрейт 21.2 Мбит/с, потеряно 30.68 % кадров
        //
        // То есть примерно каждый кадр становился ключевым. Это
        // самоусиливающаяся петля: потеря → ключевой кадр → он в
        // 6 раз крупнее → больше фрагментов → выше шанс потери
        // (арифметика находки 28) → снова ключевой.
        //
        // # Почему пауза, а не счётчик потерь
        //
        // Ключевой кадр лечит декодер **не мгновенно**: он должен
        // дойти целиком, а на плохом канале это не гарантировано.
        // Считать потери и слать по порогу — значит слать вдогонку
        // тем, что уже в пути. Пауза же прямо отвечает на вопрос
        // «дали ли мы предыдущему шанс доехать».
        //
        // 250 мс — это ~15 кадров при 60 fps, заметно меньше порога
        // видимых артефактов из критерия этапа (200 мс на восстановление
        // отводится с запасом), и заметно больше времени доставки
        // одного кадра даже на плохом канале.
        if lost_now > 0 {
            // Начало «сыплющейся» картинки. Отмечается только первая
            // потеря подряд: следующие происходят, пока изображение
            // уже сломано, и начинать отсчёт заново значило бы
            // занижать длительность артефакта.
            if broken_since.is_none() {
                broken_since = Some(now());
            }

            let since = now().duration_since(last_keyframe_request);
            if since >= KEYFRAME_COOLDOWN {
                last_keyframe_request = now();
                keyframes_requested += 1;
                if let Some(e) = encoder.as_mut() {
                    e.request_keyframe();
                }
            } else {
                // Потеря была, но ключевой уже в пути. Считаем
                // отдельно: без этого числа не видно, работает
                // ограничение или просто потерь нет.
                keyframes_suppressed += 1;
            }
        }

        // Восстановление: ключевой кадр дошёл целиком, у декодера
        // снова есть опора, и картинка перестала сыпаться.
        //
        // Считается по **доставленному** кадру, а не по отправленному:
        // отправленный ключевой может сам потеряться, и засчитать
        // восстановление по нему значило бы измерить намерение вместо
        // результата.
        if delivered.keyframe {
            if let Some(broken_at) = broken_since.take() {
                recovery_times.push(now().duration_since(broken_at));
            }
        }

        // Сверка целостности возможна только там, где есть с чем
        // сверять. У клиента журнала нет: кадры пришли от хоста, а
        // не от него самого. Тайминги в этом случае берутся из
        // самого кадра — иначе задержка мерилась бы по пути, которого
        // кадр не проходил (находка 27).
        if !direction.sends() {
            encoded.info.timings = arrival;

            // Метка захвата приехала в заголовке кадра, но она снята
            // **часами хоста**. Вычесть её из своего времени напрямую
            // нельзя: получится разница часов, а не задержка. Смещение
            // оценивает `ClockSync` по минимуму наблюдённых разниц
            // (находка 40).
            if delivered.captured_at_micros != 0 {
                // При работе через буфер наблюдение уже учтено при
                // укладке кадра — повторное исказило бы оценку
                // смещения часов. Здесь нужна только текущая
                // задержка, поэтому считаем её по готовому смещению.
                let network_delay = if use_jitter_buffer {
                    (epoch.stamp_now().as_micros() as i64
                        - delivered.captured_at_micros as i64
                        - clock_sync.offset_micros())
                    .max(0) as u64
                } else {
                    clock_sync.observe(delivered.captured_at_micros, epoch.stamp_now())
                };

                // Отметка `Captured` восстанавливается в НАШЕЙ шкале:
                // момент получения минус измеренная задержка. Так весь
                // остальной расчёт задержки работает без изменений, а
                // разница часов в него не попадает.
                if clock_sync.is_settled() {
                    let captured_local =
                        epoch.stamp_now().as_micros().saturating_sub(network_delay);
                    encoded.info.timings.mark(
                        Stage::Captured,
                        bd_core::time::Timestamp::from_micros(captured_local),
                    );
                }
            }

            encoded.data = delivered.data;
            // Дальше кадр идёт в декодер общим путём.
        } else {
            match sent_log.front() {
                Some((seq, timings, original)) if *seq == delivered.sequence => {
                    if delivered.data != *original {
                        // Диагностика, а не просто отказ: расхождение длин
                        // и расхождение содержимого — разные дефекты.
                        // Первое означает потерянный или лишний фрагмент,
                        // второе — порчу байтов.
                        let first_diff = delivered
                            .data
                            .iter()
                            .zip(original.iter())
                            .position(|(a, b)| a != b);
                        println!(
                            "ОШИБКА: транспорт исказил кадр {}: пришло {} Б, ожидалось {} Б, \
                         первое расхождение на позиции {:?}",
                            delivered.sequence,
                            delivered.data.len(),
                            original.len(),
                            first_diff
                        );
                        break;
                    }
                    // Тайминги того кадра, что дошёл, плюс момент его
                    // сборки на приёме. Задержка считается по ним.
                    encoded.info.timings = *timings;
                    if let Some(received) = arrival.get(Stage::Received) {
                        encoded.info.timings.mark(Stage::Received, received);
                    }
                    sent_log.pop_front();
                }
                _ => {
                    println!(
                        "ОШИБКА: пришёл кадр {} без отправленного оригинала",
                        delivered.sequence
                    );
                    break;
                }
            }
            encoded.data = delivered.data;
        }

        let decoded = match decoder.decode(&encoded) {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(err) if err.needs_keyframe() => {
                // Потеря синхронизации лечится ключевым кадром, а не
                // пересозданием декодера (CLAUDE.md §5.2).
                keyframes_requested += 1;
                if let Some(e) = encoder.as_mut() {
                    e.request_keyframe();
                }
                continue;
            }
            Err(err) => {
                println!("Ошибка декодирования: {err}");
                break;
            }
        };

        // Настоящее разрешение, а не выровненное декодером: показывать
        // служебное дополнение нельзя (CLAUDE.md §0.1, находка 18).
        let mut timings = decoded.info().timings;
        window.present_frame(
            decoded.texture(),
            decoded.subresource(),
            size,
            &mut timings,
            &overlay_lines,
        )?;
        presented += 1;

        if let Some(total) = timings.total() {
            total_times.push(total);
        }
        for (stage, duration) in timings.breakdown() {
            stage_times
                .entry(stage)
                .or_insert_with(|| LatencyWindow::new(4000))
                .push(duration);
        }

        // Оверлей поверх видео (§10.4). Пересчёт четыре раза в
        // секунду: чаще — мельтешение, реже — заметное запаздывание.
        if now().duration_since(last_overlay) >= overlay_period {
            last_overlay = now();
            overlay_lines.clear();

            let elapsed = now().duration_since(started).as_secs_f64().max(0.001);
            overlay_lines.push(format!("BetterDesk  {presented} кадров"));
            overlay_lines.push(format!("fps      {:>7.1}", presented as f64 / elapsed));

            if let (Some(med), Some(p95)) = (total_times.median(), total_times.p95()) {
                overlay_lines.push(format!(
                    "задержка {:>7.2} мс   p95 {:.2}",
                    med.as_secs_f64() * 1000.0,
                    p95.as_secs_f64() * 1000.0
                ));
            }

            for (stage, window) in &stage_times {
                if let Some(med) = window.median() {
                    overlay_lines.push(format!(
                        "  {:<7} {:>5.2} мс",
                        stage.label(),
                        med.as_secs_f64() * 1000.0
                    ));
                }
            }

            let stats = transport.stats();
            overlay_lines.push(format!(
                "битрейт  {:>7.1} Мбит/с",
                stats.bitrate(now().duration_since(started)) / 1_000_000.0
            ));

            // RTT (§10.4). У QUIC он измерен, у заглушки — пересчитан
            // из профиля, и это помечено: цифра, не являющаяся
            // измерением, не должна выглядеть измерением.
            let rtt = transport.rtt();
            if transport.rtt_is_measured() {
                overlay_lines.push(format!("RTT      {:>7.1} мс", rtt.as_secs_f64() * 1000.0));
            } else {
                overlay_lines.push(format!(
                    "RTT      {:>7.1} мс (профиль)",
                    rtt.as_secs_f64() * 1000.0
                ));
            }

            // Потери — двумя строками, а не одной.
            //
            // Датаграмы и кадры теряются в РАЗНОЙ пропорции, и это не
            // мелочь оформления: 3 % потерянных датаграмов дают 51 %
            // потерянных кадров, потому что кадр 1080p идёт двумя
            // десятками фрагментов и гибнет весь от потери одного
            // (находка 28). Одна строка «потери 3 %» скрывала бы ровно
            // тот факт, ради которого FEC переведён в обязательные.
            overlay_lines.push(format!(
                "потери дг{:>7.2} %   {} шт.",
                stats.datagram_loss() * 100.0,
                stats.datagrams_dropped
            ));
            let frame_loss = if stats.frames_sent > 0 {
                lost_frames as f64 / stats.frames_sent as f64 * 100.0
            } else {
                0.0
            };
            overlay_lines.push(format!("потери кд{frame_loss:>7.2} %   {lost_frames} шт."));

            // Джиттер-буфер — только там, где он работает.
            //
            // Пустые строки «удержание 0.0» у хоста и на заглушке
            // читались бы как «буфер работает и держит ноль», хотя
            // буфера там нет вовсе. Разница между «нечего показывать»
            // и «показываю ноль» — это разница между находкой 26
            // («ноль срабатываний — не работает, а неизвестно») и
            // настоящим измерением.
            if use_jitter_buffer {
                overlay_lines.push(format!(
                    "буфер    {:>7.1} мс   {} кадр.",
                    jitter_buffer.hold().as_secs_f64() * 1000.0,
                    jitter_buffer.queued()
                ));
                // Фактическое удержание рядом с целевым: расхождение
                // между ними означает, что кадры забирают не вовремя,
                // и именно оно вскрыло дефект из находки 42.
                let held = jitter_buffer.average_held().as_secs_f64() * 1000.0;
                overlay_lines.push(format!(
                    "  факт   {held:>7.1} мс   опозд. {}",
                    jitter_buffer.late_dropped()
                ));
            }

            // Состояние соединения. Молча замерший оверлей при обрыве
            // выглядит как зависший пайплайн — надо различать.
            if transport.is_real_network() && !transport.is_connected() {
                overlay_lines.push("СОЕДИНЕНИЕ ПОТЕРЯНО".to_string());
            }

            overlay_lines.push("F9 — скрыть".to_string());
        }

        // Раз в секунду — та же цифра в консоль: оверлея не видно
        // на скриншоте прогона, а сравнивать прогоны надо.
        if now().duration_since(last_report) >= Duration::from_secs(1) {
            last_report = now();
            if let Some(med) = total_times.median() {
                println!(
                    "  {presented:>5} кадров, задержка {:.2} мс",
                    med.as_secs_f64() * 1000.0
                );
            }
        }
    }

    // Сессия закончилась — всё, что оставалось зажатым, надо отпустить.
    //
    // Это не украшение отчёта, а обязанность: человек за хостом
    // остался бы с зажатым Ctrl или кнопкой мыши, и починить это он
    // может только сам, вслепую. Отпускается напрямую, минуя
    // транспорт: канала уже нет, а событие должно дойти.
    // Запоминается ДО отпускания: иначе отчёт ниже всегда показывал бы
    // «зажатых нет» — мы же их только что и отпустили. Отчёт должен
    // говорить о том, что было в конце сессии, а не после уборки.
    let stuck_at_end = host_tracker.pressed_key_count();

    if !host_tracker.is_idle() {
        let pending: Vec<_> = host_tracker
            .release_events()
            .map(|e| bd_bench::input_bridge::with_position(e, last_cursor))
            .collect();
        println!("\nОтпускаю {} зажатых после сессии", pending.len());
        if inject_input {
            // Пакетом, а не по одному: SendInput вставляет массив
            // атомарно, и между отпусканием Ctrl и C не вклинится
            // чужой ввод.
            if let Err(e) = injector.inject_batch(&pending) {
                println!("  не удалось отпустить: {e}");
            }
        }
        host_tracker.reset();
    }

    let elapsed = now().duration_since(started);

    println!("\n=== Результат ===");
    if closed_early {
        println!("(окно закрыто пользователем)");
    }

    if presented == 0 {
        println!("Ни одного кадра не показано.");
        println!("Закодировано: {encoded_frames}, пропущено: {skipped}");
        println!("Если экран не менялся — это норма, повторите с движением.");
        return Ok(());
    }

    let secs = elapsed.as_secs_f64();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;

    println!("Время замера:      {secs:.1} с");
    println!(
        "Показано кадров:   {presented} ({:.1} fps)",
        presented as f64 / secs
    );
    println!("Пропущено:         {skipped} (менялся только курсор)");
    println!("Восстановлений:    {recoveries} (смен разрешения: {resolution_changes})");

    println!(
        "Ограничитель:      пропущено {} кадров, закодировано {}",
        limiter.skipped(),
        limiter.passed()
    );

    // Ключевые кадры и размеры: диагностика просадок качества.
    println!("\nКлючевые кадры:");
    println!("  запрошено: {keyframes_requested}, выдано: {keyframes_emitted}");
    if keyframes_emitted > 0 {
        let per_min = keyframes_emitted as f64 / elapsed.as_secs_f64() * 60.0;
        println!("  частота:   {per_min:.1} в минуту");
    }
    // Подавленные запросы печатаются рядом с выданными: без этой
    // строки нельзя отличить «пауза работает» от «потерь не было»,
    // а именно она защищает от возврата шторма ключевых кадров.
    if keyframes_suppressed > 0 {
        println!(
            "  подавлено паузой: {keyframes_suppressed} (пауза {} мс)",
            KEYFRAME_COOLDOWN.as_millis()
        );
        println!("  ↑ это защита от шторма: без неё каждая потеря");
        println!("    требовала бы ключевого кадра, а тот крупнее в разы");
    }

    // Восстановление после потерь — критерий этапа 3.
    if !recovery_times.is_empty() {
        let mut sorted = recovery_times.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        let worst = *sorted.last().unwrap_or(&Duration::ZERO);
        let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];

        // Что именно измерено — говорится прямо, потому что цифра
        // легко читается как «столько времени картинка была рваной»,
        // а это НЕ так.
        //
        // Меряется интервал «потеря → первый дошедший ключевой кадр».
        // Декодер при этом не останавливается: он продолжает
        // декодировать разностные кадры, а повреждение затухает по
        // мере прихода интра-макроблоков. То есть настоящая видимая
        // порча короче измеренного интервала, иногда сильно.
        //
        // Значит, это **верхняя оценка**, а не длительность артефакта.
        // Честная проверка критерия — глазом или камерой (§10.4);
        // здесь же цифра, которая гарантированно не занижает.
        println!("\nВосстановление после потерь ({} случаев):", sorted.len());
        println!("  (интервал «потеря → дошедший ключевой»; это верхняя");
        println!("   оценка: декодер не стоит, порча затухает раньше)");
        println!("  медиана: {:.0} мс", median.as_secs_f64() * 1000.0);
        println!("  p95:     {:.0} мс", p95.as_secs_f64() * 1000.0);
        println!("  худшее:  {:.0} мс", worst.as_secs_f64() * 1000.0);

        // Критерий сформулирован про **видимые артефакты**, а не про
        // средний случай: одна затяжная пауза заметна, даже если
        // медиана прекрасна. Поэтому вердикт выносится по p95.
        //
        // Но p95 на десятке наблюдений — это просто максимум, и
        // выносить по нему вердикт значило бы принять один выброс за
        // свойство канала. Меньше 30 случаев — говорим об этом прямо,
        // а не печатаем уверенный крестик.
        if sorted.len() < 30 {
            println!(
                "  ⚠ случаев мало ({}) — p95 здесь почти равен максимуму,",
                sorted.len()
            );
            println!("    и один выброс определяет всю картину. Для вердикта");
            println!("    нужен прогон подлиннее или канал похуже.");
        } else if p95.as_millis() <= 200 {
            println!("  ✅ критерий этапа 3 выполнен (p95 ≤ 200 мс)");
        } else {
            println!("  ❌ ВЫШЕ 200 мс — артефакты видны дольше нормы");
            // Объяснение печатается рядом с вердиктом, потому что
            // причина не в паузе между запросами и подбором её не
            // лечится — это проверено замерами (см. таблицу у
            // KEYFRAME_COOLDOWN).
            //
            // Настоящая причина — размер самого ключевого кадра.
            // Причин у превышения две, и это разные диагнозы.
            // Печатать надо ту, которая подтверждается цифрами
            // этого прогона, а не заготовленную (находка 34).
            let delivered_share = if keyframes_emitted > 0 {
                sorted.len() as f64 / keyframes_emitted as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "     Ключевых выдано {keyframes_emitted}, восстановлений {} ({delivered_share:.0} %).",
                sorted.len()
            );

            if delivered_share < 80.0 {
                // Ключевые кадры не доезжают — это арифметика, и
                // подбором паузы она не лечится.
                println!("     Больше половины ключевых кадров сами теряются:");
                println!("     кадр идёт десятками фрагментов, и при 1 % потерь");
                println!("     шанс дойти целиком около половины (находка 28).");
                println!("     Лечится FEC — этап 6, а не подбором паузы.");
            } else {
                // Ключевые доезжают, но отдельные случаи затягиваются.
                // На малой выборке p95 — это фактически максимум, и
                // один выброс задаёт весь вердикт.
                println!("     Ключевые доезжают, но отдельные восстановления");
                println!(
                    "     затягиваются. При {} случаях p95 — это почти",
                    sorted.len()
                );
                println!("     максимум, и один выброс определяет вердикт.");
                println!("     Смотреть на медиану и на длину прогона.");
            }
        }
    } else if lost_frames > 0 {
        // Потери были, а восстановлений не зафиксировано: значит
        // ключевой кадр ни разу не дошёл целиком. Это хуже долгого
        // восстановления, и молчать об этом нельзя.
        println!("\n⚠ Потери были ({lost_frames}), но ни один ключевой кадр");
        println!("  не дошёл целиком — измерить восстановление нечем.");
    }

    let median_of = |v: &mut Vec<usize>| -> usize {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[v.len() / 2]
    };
    let key_med = median_of(&mut key_bytes);
    let delta_med = median_of(&mut delta_bytes);
    if key_med > 0 && delta_med > 0 {
        println!("  медиана ключевого:   {key_med} Б");
        println!("  медиана разностного: {delta_med} Б");
        println!(
            "  ключевой тяжелее в {:.1}×",
            key_med as f64 / delta_med as f64
        );
        // Бюджет одного кадра при заданном битрейте и 60 fps — это
        // и есть размер VBV (§5.2). Ключевой кадр крупнее него не
        // помещается, и энкодер огрубляет картинку, чтобы уложиться.
        // Настройки берутся из `encoder_config`, а не из энкодера:
        // у клиента энкодера нет, а блок сюда всё равно не попадёт
        // (у него нет и закодированных кадров). Компилятор этого не
        // знает, и `unwrap` здесь был бы обещанием, которое некому
        // проверить.
        let vbv = target_bitrate as f64 / 60.0 / 8.0;
        println!("  бюджет кадра (VBV):  {vbv:.0} Б");
        if key_med as f64 > vbv {
            println!("  ⚠ ключевой кадр НЕ помещается в VBV — это и есть");
            println!("    источник просадки качества на ключевых кадрах");
        }
    }

    let stats = transport.stats();
    println!("\nТранспорт (профиль «{link_name}»):");
    println!(
        "  кадров:      отправлено {}, доставлено {}, потеряно {}",
        stats.frames_sent, stats.frames_received, lost_frames
    );
    println!(
        "  датаграмов:  {} шт., потеряно {} ({:.2} %)",
        stats.datagrams_sent,
        stats.datagrams_dropped,
        stats.datagram_loss() * 100.0
    );
    println!(
        "  битрейт:     {:.1} Мбит/с",
        stats.bitrate(elapsed) / 1_000_000.0
    );
    if stats.frames_sent > 0 {
        println!(
            "  фрагментов на кадр: {:.1}",
            stats.datagrams_sent as f64 / stats.frames_sent as f64
        );
        // Доля потерянных кадров рядом с долей потерянных датаграмов:
        // они расходятся в разы, и видеть надо обе (находка 28).
        println!(
            "  потеряно кадров: {:.2} % (датаграмов {:.2} %)",
            lost_frames as f64 / stats.frames_sent as f64 * 100.0,
            stats.datagram_loss() * 100.0
        );
    }
    // RTT — та же цифра, что на оверлее. Печатается здесь именно
    // затем, чтобы её можно было сверить: расхождение между оверлеем
    // и отчётом — первый признак, что одна из них считается не так
    // (находка 34).
    let rtt_ms = transport.rtt().as_secs_f64() * 1000.0;
    if transport.rtt_is_measured() {
        println!("  RTT:         {rtt_ms:.1} мс (измерен QUIC)");
    } else {
        println!("  RTT:         {rtt_ms:.1} мс (пересчёт профиля, не измерение)");
    }
    // Джиттер-буфер (этап 3). Печатается только там, где работает:
    // у хоста и на заглушке его нет, и пустая строка вводила бы
    // в заблуждение.
    if use_jitter_buffer {
        println!("\nДжиттер-буфер:");
        println!(
            "  удержание: целевое {:.1} мс, фактическое {:.1} мс",
            jitter_buffer.hold().as_secs_f64() * 1000.0,
            jitter_buffer.average_held().as_secs_f64() * 1000.0
        );
        println!(
            "  в очереди на конец: {}, опоздавших отброшено: {}",
            jitter_buffer.queued(),
            jitter_buffer.late_dropped()
        );
        if jitter_buffer.overflow_dropped() > 0 {
            // Переполнение означает, что показ не успевает за
            // приёмом. Это не про сеть, а про нас.
            println!(
                "  ⚠ отброшено переполнением: {} (показ не успевает)",
                jitter_buffer.overflow_dropped()
            );
        }
        if clock_sync.is_settled() {
            println!(
                "  смещение часов сторон: {:.1} мс ({} наблюдений)",
                clock_sync.offset_micros() as f64 / 1000.0,
                clock_sync.samples()
            );
        } else {
            // Меньше 30 кадров — оценка смещения ещё не устоялась,
            // и показывать по ней задержку было бы выдумкой.
            println!(
                "  ⚠ оценка часов не устоялась ({} наблюдений) — \
                 задержка ниже недостоверна",
                clock_sync.samples()
            );
        }
    }

    // Курсор (этап 2). Печатается всегда: нулевая строка отличает
    // «курсор не двигался» от «курсор сломан».
    println!("\nКурсор:");
    if cursor_updates == 0 {
        println!("  обновлений не было (мышь не двигалась)");
    } else {
        println!(
            "  позиций: {cursor_updates}, форм отправлено: {cursor_shapes_sent}, \
             применено: {cursor_shapes_applied}"
        );
        if cursor_rejected > 0 {
            // Потери здесь не страшны: следующее движение мыши
            // пришлёт позицию заново. Форма — хуже, но и она придёт
            // при следующей смене.
            println!("  отброшено (потери или неверный формат): {cursor_rejected}");
        }
        match window.cursor_shape_id() {
            Some(id) => println!("  ✅ форма загружена (номер {id})"),
            None => println!("  ⚠ форма так и не загрузилась — курсор не рисуется"),
        }
    }

    // Ввод (этап 2). Печатается всегда, даже когда событий не было:
    // нулевая строка отличает «ввод не проверялся» от «ввод сломан»,
    // а отсутствие строки не отличает ничего.
    println!("\nВвод:");
    if input_sequence == 0 {
        println!("  событий не было (окно не получало ввода)");
    } else {
        println!("  отправлено: {input_sent}, применено: {input_applied}");
        if input_redundant > 0 {
            println!("  подавлено дубликатов (автоповтор): {input_redundant}");
        }
        if input_lost > 0 {
            // Потеря события ввода опаснее потери кадра: потерянное
            // отпускание клавиши оставляет её зажатой навсегда (§5.3).
            println!("  ⚠ потеряно в канале: {input_lost}");
        }
        if input_blocked > 0 {
            // UIPI: окно с большим уровнем целостности не принимает
            // наш ввод. Не сбой, но пользователю надо это видеть.
            println!("  заблокировано системой (UIPI): {input_blocked}");
        }
        let dropped = window.input_dropped();
        if dropped > 0 {
            println!("  ⚠ вытеснено из очереди окна: {dropped} (цикл не успевал)");
        }
        if stuck_at_end > 0 {
            // Главная проверка критерия «ни одна клавиша не залипает».
            // Это не обязательно дефект: если прогон оборван, пока
            // клавиша была зажата, остаток закономерен. Дефект — когда
            // клавиши копятся при обычной работе.
            println!("  ⚠ на конец сессии оставалось зажатых: {stuck_at_end}");
            println!("    (норма, если прогон оборван с зажатой клавишей)");
        } else {
            println!("  ✅ на хосте не осталось зажатых клавиш");
        }
    }

    if !queue_depth.is_empty() {
        let mut sorted = queue_depth.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        println!("  очередь датаграмов: медиана {median}, максимум {max_queue_depth}");
        // Кадр идёт несколькими фрагментами, поэтому очередь в единицы
        // — норма. Десятки означают, что приёмник не успевает за
        // отправителем, и стадия `network` показывает ожидание своей
        // очереди, а не задержку канала.
        if median > 32 {
            println!("  ⚠ очередь глубокая: `network` мерит ожидание, не канал");
        }
    }
    if link.loss == 0.0 {
        println!("  Профиль без потерь: ветка неполного кадра в сборщике");
        println!("  не исполнялась. Проверять её надо --link lan/mobile,");
        println!("  иначе ноль потерь значит «неизвестно» (§0.1, находка 26).");
    }

    if presented as f64 / secs > 90.0 {
        println!("  fps выше частоты экрана: окно вывода стоит на");
        println!("  захватываемом мониторе и питает само себя. На цифру");
        println!("  задержки это не влияет — каждый кадр измеряется");
        println!("  отдельно, — но сам fps в таком прогоне не значим.");
    }

    println!("\nРазбивка по стадиям (медиана / p95):");
    for (stage, window) in &stage_times {
        if let (Some(med), Some(p95)) = (window.median(), window.p95()) {
            println!(
                "  {:<9} {:>6.2} мс  {:>6.2} мс",
                stage.label(),
                ms(med),
                ms(p95)
            );
        }
    }
    println!("  Декод и рендер здесь — стоимость вызова: GPU работает");
    println!("  асинхронно. Их настоящая цена — в decode_probe (0.2 мс)");
    println!("  и проявится в glass-to-glass, а не в этих отметках.");

    let (Some(median), Some(p95), Some(max)) =
        (total_times.median(), total_times.p95(), total_times.max())
    else {
        println!("\nНедостаточно данных для итога.");
        return Ok(());
    };

    println!("\nЗАДЕРЖКА ЗАХВАТ → ПОКАЗ:");
    println!("  медиана: {:.2} мс", ms(median));
    println!("  p95:     {:.2} мс", ms(p95));
    println!("  максимум:{:.2} мс", ms(max));

    // Критерий выхода из этапа 1 (docs/roadmap.md).
    //
    // Порог 25 мс задан **для localhost**. На профиле с эмулированной
    // задержкой сравнивать с ним нельзя: там задержка канала внесена
    // нарочно, и вердикт «выше 25 мс» означал бы лишь, что эмуляция
    // работает. Поэтому оценивается задержка за вычетом канала —
    // то есть цена самого пайплайна.
    println!();
    let link_ms = link.delay.as_secs_f64() * 1000.0;

    // Вычитается ФАКТИЧЕСКАЯ стадия `network`, а не заявленная
    // профилем задержка. Разница между ними бывает большой, и раньше
    // отчёт молча приписывал её пайплайну.
    //
    // Почему они расходятся: при ненулевой задержке кадр не готов
    // в той же итерации, где отправлен, а `loopback` делает один
    // `receive()` на один `send()`. Кадр забирается на следующей
    // итерации, то есть к задержке канала добавляется шаг цикла
    // кодирования (~17 мс при 60 fps). На профиле `lan` это давало
    // 20 мс вместо объявленной 1 мс, и 19 мс из них уходили в счёт
    // пайплайна, который их не тратил.
    //
    // Это ожидание — свойство пробы, а не продукта: в реальном
    // клиенте приём идёт своим потоком и не ждёт такта энкодера.
    // Значит, вычитать надо всё время между `Sent` и `Received`.
    let measured_network_ms = stage_times
        .get(&Stage::Received)
        .and_then(|w| w.median())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(link_ms);
    let pipeline_ms = (ms(median) - measured_network_ms).max(0.0);

    if link.delay.is_zero() {
        if pipeline_ms <= 25.0 {
            println!("  ✅ КРИТЕРИЙ ЭТАПА 1 ВЫПОЛНЕН (≤ 25 мс)");
        } else {
            println!("  ❌ ВЫШЕ 25 мс — разбираться, а не идти дальше (§11)");
        }
    } else {
        println!("  Профиль «{link_name}» объявляет {link_ms:.0} мс задержки канала,");
        println!("  фактически стадия network заняла {measured_network_ms:.2} мс.");
        if measured_network_ms > link_ms * 2.0 + 1.0 {
            // Расхождение штатное и объяснимое: проба делает один
            // `receive()` на один `send()`, поэтому кадр забирается
            // на следующей итерации. Молчать о нём нельзя — иначе
            // разница выглядит как необъяснимая задержка сети.
            println!("  Разница — ожидание следующей итерации: проба берёт");
            println!("  один кадр за такт кодирования, а не своим потоком.");
        }
        println!("  Критерий этапа 1 задан для localhost, поэтому здесь");
        println!("  оценивается пайплайн без сети: {pipeline_ms:.2} мс.");
        if pipeline_ms <= 25.0 {
            println!("  ✅ сам пайплайн в бюджете (≤ 25 мс)");
        } else {
            println!("  ❌ пайплайн вне бюджета даже без канала (§11)");
        }
    }

    println!();
    println!("Это не glass-to-glass: не учтено ожидание кадра до захвата");
    println!("и задержка самого дисплея (docs/latency-budget.md §2).");
    println!("Честная проверка — камера на 240 fps, §10.4.");

    Ok(())
}

/// Разобрать `--seconds N` из аргументов.
#[cfg(windows)]
fn parse_seconds() -> Option<u64> {
    parse_arg("--seconds")
}

/// Разобрать `--monitor N` из аргументов.
#[cfg(windows)]
fn parse_monitor() -> Option<u32> {
    parse_arg("--monitor")
}

/// Разобрать `--link ИМЯ` из аргументов.
///
/// Неизвестное имя не прерывает прогон: проба — инструмент замера, и
/// падать из-за опечатки в необязательном флаге ей незачем. Но и молча
/// подставлять другой профиль нельзя — цифры прогона стали бы ложью,
/// поэтому подмена печатается.
#[cfg(windows)]
fn parse_link() -> (&'static str, bd_transport::LinkProfile) {
    use bd_transport::LinkProfile;

    let Some(name) = parse_arg::<String>("--link") else {
        return ("perfect", LinkProfile::PERFECT);
    };
    match name.as_str() {
        "perfect" => ("perfect", LinkProfile::PERFECT),
        "lan" => ("lan", LinkProfile::LAN),
        "internet" => ("internet", LinkProfile::INTERNET),
        "mobile" => ("mobile", LinkProfile::MOBILE),
        "lossy" => ("lossy", LinkProfile::LOSSY),
        other => {
            println!("Неизвестный профиль канала «{other}», взят perfect.");
            println!("Доступны: perfect, lan, internet, lossy, mobile.\n");
            ("perfect", LinkProfile::PERFECT)
        }
    }
}

/// Значение именованного аргумента.
#[cfg(windows)]
fn parse_arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next()?.parse().ok();
        }
    }
    None
}

/// Окно-«агитатор»: само меняет пиксели на захватываемом мониторе.
///
/// Нужно для долгих прогонов на стабильность. DXGI отдаёт кадр только
/// при изменении экрана (находка 8), поэтому 30-минутный прогон
/// требовал, чтобы человек полчаса шевелил окна руками. Это и есть
/// причина, по которой критерий держался открытым дольше всех
/// остальных: не потому, что код не готов, а потому, что проверку
/// было дорого проводить.
///
/// Агитатор рисует движущийся прямоугольник в маленьком окне без рамки.
/// Он намеренно **не** участвует в измерении: это источник изменений
/// на экране, а не часть пайплайна. Цифры прогона от него не зависят,
/// кроме того, что кадры вообще появляются.
#[cfg(windows)]
struct Agitator {
    hwnd: ::windows::Win32::Foundation::HWND,
    step: u32,
}

#[cfg(windows)]
impl Agitator {
    /// Создать окно в левом верхнем углу захватываемого монитора.
    ///
    /// `origin` — координаты монитора в виртуальном рабочем столе:
    /// окно обязано оказаться на том экране, который захватывается,
    /// иначе изменений в кадре не будет и смысла в агитаторе тоже.
    fn new(origin: (i32, i32)) -> ::windows::core::Result<Self> {
        use ::windows::core::w;
        use ::windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SW_SHOWNA,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
        };

        const SIZE: i32 = 160;

        // Класс STATIC берётся готовый: своя оконная процедура здесь
        // не нужна, рисуем поверх через GDI.
        //
        // WS_EX_NOACTIVATE обязателен: окно не должно перехватывать
        // фокус, иначе оно будет мешать работать на машине, где идёт
        // получасовой прогон.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("bd-agitator"),
                WS_POPUP | WS_VISIBLE,
                origin.0 + 16,
                origin.1 + 16,
                SIZE,
                SIZE,
                None,
                None,
                None,
                None,
            )
        }?;

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE
                    | ::windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | ::windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
            );
        }

        Ok(Self { hwnd, step: 0 })
    }

    /// Изменить картинку. Вызывать каждую итерацию цикла.
    ///
    /// Заливка целиком, а не сдвиг: DXGI отслеживает изменившиеся
    /// области, и мелкое изменение дало бы кадр, но не нагрузку,
    /// похожую на настоящую работу.
    fn agitate(&mut self) {
        use ::windows::Win32::Foundation::{COLORREF, RECT};
        use ::windows::Win32::Graphics::Gdi::{
            CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC, HBRUSH,
        };

        self.step = self.step.wrapping_add(7);

        // Цвет гуляет по кругу: постоянная заливка одним цветом после
        // первого кадра перестала бы быть изменением.
        let phase = self.step;
        let color = COLORREF(
            (phase & 0xFF)
                | ((phase.wrapping_mul(3) & 0xFF) << 8)
                | ((phase.wrapping_mul(5) & 0xFF) << 16),
        );

        unsafe {
            let dc = GetDC(Some(self.hwnd));
            if dc.is_invalid() {
                return;
            }
            let brush = CreateSolidBrush(color);
            if !brush.is_invalid() {
                // Полоса ездит вниз-вверх: так меняется не только цвет,
                // но и геометрия — ближе к настоящей работе с окнами.
                let offset = (self.step / 4 % 120) as i32;
                let rect = RECT {
                    left: 0,
                    top: offset,
                    right: 160,
                    bottom: offset + 40,
                };
                FillRect(dc, &rect, HBRUSH(brush.0));
                let _ = DeleteObject(brush.into());
            }
            ReleaseDC(Some(self.hwnd), dc);
        }
    }
}

#[cfg(windows)]
impl Drop for Agitator {
    fn drop(&mut self) {
        use ::windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
        // SAFETY: окно создано в этом же типе и ещё не разрушено.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Присутствует ли флаг без значения в аргументах.
#[cfg(windows)]
fn parse_flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

/// Роль в QUIC-соединении, если оно запрошено.
#[cfg(windows)]
enum QuicRole {
    /// Слушать на этом адресе и ждать клиента.
    Host(std::net::SocketAddr),
    /// Подключиться к этому адресу.
    Client(std::net::SocketAddr),
}

/// Разобрать `--quic-host АДРЕС` или `--quic-connect АДРЕС`.
///
/// Без них берётся заглушка: она остаётся основным инструментом для
/// замеров без сети, а QUIC включается явно.
#[cfg(windows)]
fn parse_quic_role() -> Option<QuicRole> {
    if let Some(addr) = parse_arg::<String>("--quic-host") {
        return match addr.parse() {
            Ok(addr) => Some(QuicRole::Host(addr)),
            Err(e) => {
                // Опечатка в адресе не должна молча превращаться
                // в прогон через заглушку: цифры получились бы
                // совсем другие, а причина неочевидна.
                println!("Неверный адрес для --quic-host ({e}), беру заглушку.");
                None
            }
        };
    }
    if let Some(addr) = parse_arg::<String>("--quic-connect") {
        return match addr.parse() {
            Ok(addr) => Some(QuicRole::Client(addr)),
            Err(e) => {
                println!("Неверный адрес для --quic-connect ({e}), беру заглушку.");
                None
            }
        };
    }
    None
}
