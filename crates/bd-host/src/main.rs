//! Агент хоста: захват, энкод, отправка, инжект ввода.
//!
//! # Чем отличается от пробы `loopback`
//!
//! `loopback` совмещает обе роли в одном процессе: он и шлёт, и
//! принимает, и показывает окно. Хосту окно не нужно — он ничего не
//! показывает, он отдаёт свой экран.
//!
//! Практическое следствие: хост не создаёт ни swapchain, ни декодера.
//! Это снимает с него требование уметь декодировать (у пробы оно было,
//! потому что она замыкала путь на себя) и убирает петлю обратной
//! связи, из-за которой окно вывода на захватываемом экране гнало
//! DXGI на предельной частоте (находка 25).
//!
//! # Что осталось от пробы дословно
//!
//! Обработка `ACCESS_LOST` и secure desktop: любая ошибка захвата —
//! повод подождать, а не выйти, и сдаёмся только по таймауту в минуту.
//! Отличать «временно нельзя» от «сломано навсегда» по коду ошибки
//! ненадёжно — полного списка кодов secure desktop нет (находка 36в).
//!
//! На этапах 1–4 это обычное приложение. Служба Windows появляется
//! на этапе 8 отдельным бинарём (CLAUDE.md §3.2) — не усложнять раньше.
//!
//! Запуск: `cargo run --release -p bd-host -- --listen 0.0.0.0:7000`

#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    println!("BetterDesk работает только на Windows (CLAUDE.md §3.1).");
}

#[cfg(windows)]
mod any_encoder;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    run::main()
}

#[cfg(windows)]
mod run {
    use crate::any_encoder::AnyEncoder;
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};
    use bd_codec::{EncoderConfig, FrameKind};
    use bd_core::device::DeviceId;
    use bd_core::input::SequencedInput;
    use bd_core::metrics::FrameTimings;
    use bd_core::pacing::FrameLimiter;
    use bd_core::time::{now, Epoch};
    use bd_input::windows::InputInjector;
    use bd_input::InputTracker;
    use bd_transport::{PayloadKind, QuicTransport, SignalEvent, Signaling};
    use std::net::SocketAddr;
    use std::time::Duration;

    /// Таймаут ожидания кадра от DXGI.
    ///
    /// `WAIT_TIMEOUT` означает «экран не изменился» и ошибкой не
    /// является (§5.1). 16 мс — примерно кадр при 60 Гц: дольше ждать
    /// значит запаздывать, короче — жечь процессор на опросе.
    const CAPTURE_TIMEOUT: Duration = Duration::from_millis(16);

    /// Сколько ждать подключения клиента.
    const ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);

    /// Пауза между запросами ключевого кадра.
    ///
    /// Значение выбрано замером, а не на глаз (находка 52): при
    /// запросе на каждую потерю получалось 707 ключевых в минуту и
    /// битрейт 21 Мбит/с вместо 15. Петля самоусиливающаяся —
    /// ключевой кадр крупнее, а значит теряется охотнее.
    const KEYFRAME_COOLDOWN: Duration = Duration::from_millis(120);

    pub fn main() -> anyhow::Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();

        let bind = match parse_listen() {
            Some(addr) => addr,
            None => {
                println!("BetterDesk — хост\n");
                println!("Запуск:");
                println!("  cargo run --release -p bd-host -- --listen 0.0.0.0:7000\n");
                println!("Дополнительно:");
                println!("  --monitor N       какой экран отдавать (по умолчанию основной)");
                println!("  --fps N           предел частоты кодирования (по умолчанию 60)");
                println!("  --inject-input    применять ввод клиента (по умолчанию НЕТ)");
                println!("  --seconds N       завершиться через N с (для замеров)");
                println!();
                println!("На клиенте при этом:");
                println!("  cargo run --release -p bd-client -- --connect АДРЕС:7000");
                return Ok(());
            }
        };

        println!("=== BetterDesk — хост ===\n");

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

        println!(
            "Монитор {}: {} — {}x{}",
            target.index, target.device_name, target.size.width, target.size.height
        );

        // Одна эпоха на весь пайплайн (CLAUDE.md §4.5).
        let epoch = Epoch::new();

        let mut capturer = match DxgiCapturer::new(target.index, epoch) {
            Ok(c) => c,
            Err(err) => {
                println!("\n❌ Захват экрана недоступен: {err}");
                println!();
                println!("Частая причина — гибридная графика (§5.1): DXGI");
                println!("требует, чтобы приложение работало на том же GPU,");
                println!("что и дисплей. Фоллбэк на WGC запланирован.");
                println!();
                println!("Эта машина может работать клиентом (bd-client),");
                println!("но не хостом. Проверить: bd-bench --bin machine_probe");
                return Err(err.into());
            }
        };

        // Размер не константа на весь прогон: смена разрешения — самая
        // частая причина `ACCESS_LOST`, и после восстановления
        // дупликация может отдавать другой размер. Тогда энкодер
        // пересоздаётся (находка 31).
        let mut size = capturer.size();

        let encoder_config = EncoderConfig::low_latency(size, 60);
        let target_bitrate = encoder_config.rate_control.target_bitrate();
        println!(
            "Энкодер: H.264 {}x{}@60, CBR {} Мбит/с",
            size.width,
            size.height,
            target_bitrate / 1_000_000
        );

        let mut encoder = match AnyEncoder::new(capturer.device(), encoder_config, epoch) {
            Ok(e) => e,
            Err(err) => {
                println!("\n❌ Аппаратный энкодер H.264 недоступен: {err}");
                println!();
                println!("Хостом может быть только машина с аппаратным");
                println!("энкодером: NVIDIA (NVENC), Intel QuickSync или AMD.");
                println!("Софтверного пути нет намеренно — копия кадра в");
                println!("системную память стоит 5-10 мс (§4.2.3).");
                println!();
                println!("Что есть на этой машине: bd-bench --bin machine_probe");
                return Err(err.into());
            }
        };
        println!("Бэкенд: {}", encoder.backend_name());

        // Ограничитель частоты. DXGI отдаёт кадр на КАЖДОЕ изменение
        // экрана — при активной работе это 150-250 кадров/с, и все
        // они шли бы в энкодер. При CBR битрейт делится на фактическое
        // число кадров, и каждому достаётся втрое меньше бит: это
        // видно глазом как пикселизация (находка 30).
        let fps_limit = parse_fps().unwrap_or(60);
        let mut limiter = FrameLimiter::new(fps_limit);
        if fps_limit > 0 {
            println!("Предел частоты кодирования: {fps_limit} кадров/с");
        } else {
            println!("Предел частоты снят (--fps 0)");
        }

        // ОДНО соединение на всё: видео, ввод, курсор.
        //
        // # Что здесь было раньше и почему изменилось
        //
        // До этапа 4 каналов было три, на трёх портах подряд (находка
        // 56). Причина была не в приоритетах, а в сборщике: он вёл
        // один номер последнего выданного кадра на весь поток, и виды
        // с разной скоростью нумерации выбрасывали пакеты друг друга
        // как устаревшие. Разнести их по портам было дешевле, чем
        // трогать отлаженный `Reassembler`.
        //
        // На этапе 4 цена поменялась местами. Пробивание NAT требует
        // бить с того же порта, на котором пойдут данные, — то есть
        // три порта означают втрое больше пробивания и втрое больше
        // способов не пробиться. Поэтому состояние сборки разделено
        // по `PayloadKind` внутри одного соединения, а портов снова
        // один.
        //
        // Побочная выгода видна человеку, а не коду: в файрволе
        // открывается один порт вместо трёх, и исчезает симптом
        // «видео идёт, а мышь и клавиатура нет», который без
        // подсказки диагностируется долго.
        println!(
            "\nСлушаю {} (видео, ввод и курсор одним каналом).",
            bind.port()
        );
        // Объявиться на сигналинге, если он задан.
        //
        // # Почему это до приёма клиента, но не обязательно
        //
        // Регистрация должна произойти раньше, чем хост залипнет в
        // ожидании клиента: `QuicTransport::host` блокируется, и
        // после него мы бы уже ничего не успели объявить.
        //
        // При этом сигналинг **не обязателен**: в локальной сети
        // адрес известен и без него, а требовать сервер там, где он
        // не нужен, значило бы сделать продукт неработоспособным при
        // недоступном сервере (§5.4 — LAN обязан работать сам).
        //
        // `Signaling` держится в переменной до конца функции: его
        // `Drop` закрывает соединение, и хост исчез бы из реестра
        // ровно в тот момент, когда стал доступен.
        let _signaling = match parse_arg("--signaling") {
            Some(url) => {
                let id = device_id();
                println!("Регистрируюсь на сигналинге {url}...");

                match Signaling::connect(&url, Duration::from_secs(10)) {
                    Ok(sig) => {
                        // Регистрируем ДОСТИЖИМЫЙ адрес, а не адрес
                        // привязки.
                        //
                        // `--listen 0.0.0.0:7000` означает «слушаю на
                        // всех интерфейсах» — это не адрес, по
                        // которому можно постучаться. Отправив его
                        // как есть, хост заставлял клиента бить в
                        // `0.0.0.0:7000`, то есть в никуда: живой
                        // прогон показал ровно это.
                        //
                        // Для пробивания важен именно локальный адрес:
                        // сервер отдаёт его клиенту, когда обе стороны
                        // за одним NAT (тогда внешние адреса совпадают
                        // и через них они друг друга не увидят).
                        sig.register(id, bd_transport::reachable_addr(bind))?;

                        // Ждём подтверждения: без него неизвестно,
                        // принял ли сервер регистрацию, и человек
                        // диктовал бы ID, по которому его не найти.
                        match sig.wait(Duration::from_secs(5)) {
                            Ok(Some(SignalEvent::Registered { public_addr, .. })) => {
                                println!("\n╭──────────────────────────────╮");
                                println!("│  ID этого компьютера         │");
                                println!("│                              │");
                                println!("│        {id}        │");
                                println!("╰──────────────────────────────╯");
                                println!("Внешний адрес: {public_addr}");
                                println!("Продиктуйте ID тому, кто подключается.\n");
                            }
                            Ok(Some(SignalEvent::Failed { reason })) => {
                                println!("❌ Сигналинг отказал: {reason}");
                                println!("   Подключение по ID работать не будет.");
                                println!("   Прямое подключение по адресу — будет.\n");
                            }
                            Ok(_) | Err(_) => {
                                println!("⚠  Сигналинг не подтвердил регистрацию.");
                                println!("   Подключение по ID может не работать.\n");
                            }
                        }
                        Some(sig)
                    }
                    Err(e) => {
                        // Недоступный сигналинг — не повод не работать:
                        // в локальной сети он не нужен вовсе.
                        println!("⚠  {e}");
                        println!("   Работаю без него: подключение по адресу.\n");
                        None
                    }
                }
            }
            None => None,
        };

        println!("Жду клиента ({} с)...", ACCEPT_TIMEOUT.as_secs());

        // Если сигналинг есть — ждём, пока клиент объявится, и бьём
        // ему навстречу ДО того, как отдать сокет под QUIC.
        //
        // # Почему хост обязан бить первым, а не просто слушать
        //
        // Роутер клиента пропустит наши пакеты только после того, как
        // сам отправит что-то нам, — и наоборот. Ждать «входящее
        // соединение» за NAT бессмысленно: его отбросят раньше, чем
        // оно дойдёт. Поэтому обе стороны шлют одновременно, и
        // `PeerWants` — сигнал, что вторая сторона уже начала.
        //
        // Без сигналинга ничего этого не нужно: в локальной сети
        // адрес достижим напрямую.
        let mut session = match punch_towards_peer(_signaling.as_ref(), bind) {
            Some(socket) => QuicTransport::host_on_socket(socket, ACCEPT_TIMEOUT, epoch)?,
            None => QuicTransport::host(bind, ACCEPT_TIMEOUT, epoch)?,
        };
        println!("Клиент подключился.\n");

        // Инжект ввода по умолчанию ВЫКЛЮЧЕН.
        //
        // Это управление чужой машиной: включать его молча нельзя, а
        // на этапе 3 нет ни аутентификации, ни подтверждения на хосте
        // (они появятся на этапе 5, §7.3). До тех пор — явный флаг.
        let inject_input = parse_flag("--inject-input");
        let injector = InputInjector::new();
        if inject_input {
            println!("⚠  Ввод клиента ПРИМЕНЯЕТСЯ к этой машине (--inject-input).");
            println!("   Аутентификации пока нет — этап 5. Не оставляйте");
            println!("   хост доступным из недоверенной сети.\n");
        } else {
            println!("Ввод клиента принимается, но НЕ применяется.");
            println!("Включить: --inject-input\n");
        }

        // Трекер зажатых клавиш: то, что клиент зажал, надо отпустить
        // при разрыве, иначе человек за этой машиной останется с
        // залипшим Ctrl и будет чинить это вслепую (этап 2).
        let mut tracker = InputTracker::new();

        // Ограничение прогона по времени — чтобы замер между двумя
        // машинами делался одной командой и заканчивался сам
        // (та же причина, что у клиента: находка 33).
        let limit = parse_seconds().map(Duration::from_secs);
        if let Some(d) = limit {
            println!("Прогон {} с, затем итоги.", d.as_secs());
        }

        let started = now();
        let mut captured = 0u64;
        let mut encoded_frames = 0u64;
        let mut keyframes = 0u64;
        let mut input_applied = 0u64;
        let mut input_blocked = 0u64;
        let mut cursor_updates = 0u64;
        let mut cursor_shapes = 0u64;
        let mut last_keyframe_request = now();
        let mut recoveries = 0u64;

        println!("Отдаю экран. Ctrl+C — завершить.\n");

        'session: loop {
            if !session.is_connected() {
                println!("Клиент отключился.");
                break;
            }

            if limit.is_some_and(|d| now().duration_since(started) >= d) {
                println!("Время прогона вышло.");
                break;
            }

            // ── Ввод: клиент → инжект ─────────────────────────────
            //
            // Цикл, а не одна попытка: событий приходит несколько за
            // итерацию, и копить их значит копить задержку отклика.
            loop {
                let mut arrival = FrameTimings::default();
                let delivered = match session.receive(&mut arrival) {
                    Ok(Some(d)) => d,
                    Ok(None) => break,
                    Err(_) => break,
                };
                if delivered.kind != PayloadKind::Input {
                    continue;
                }

                // Испорченный пакет не рвёт сессию: он мог прийти от
                // кого угодно, аутентификации пока нет (§8.5).
                let Some(parsed) = SequencedInput::parse(&delivered.data) else {
                    continue;
                };

                tracker.track(&parsed.event);

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
                            break 'session;
                        }
                    }
                }
            }

            // ── Курсор: хост → клиент ─────────────────────────────
            //
            // Забирается ДО кадра и прокачивается каждую итерацию:
            // курсор меняется чаще, чем картинка, и именно на
            // статичном экране. Ждать кадра значило бы терять
            // движение мыши там, где оно единственное, что есть.
            if let Some(state) = capturer.take_cursor() {
                // Форма едет первой: позиция ссылается на неё номером,
                // и клиент, получивший позицию раньше формы, рисовать
                // не сможет.
                if let Some(shape) = state.new_shape {
                    let mut timings = FrameTimings::default();
                    if session
                        .send(PayloadKind::Control, false, &shape.encode(), &mut timings)
                        .is_ok()
                    {
                        cursor_shapes += 1;
                    }
                }

                // Позиция идёт видом `Cursor`, а не `Video`.
                //
                // Пока курсор ехал своим соединением, вид не значил
                // ничего — в том канале не было ничего другого. В
                // сведённом канале вид стал адресом: под кодом
                // `Video` позиция попала бы в сборку видеокадров, где
                // своя нумерация, и оба потока молча выбрасывали бы
                // друг друга как устаревшие.
                let mut timings = FrameTimings::default();
                if session
                    .send(
                        PayloadKind::Cursor,
                        false,
                        &state.position.encode(),
                        &mut timings,
                    )
                    .is_ok()
                {
                    cursor_updates += 1;
                }
            }

            // ── Видео: захват → энкод → отправка ──────────────────
            let frame = match capturer.next_frame(CAPTURE_TIMEOUT) {
                Ok(CaptureOutcome::Frame(f)) => f,
                // Экран не изменился. Хосту показывать нечего, и
                // слать тоже: неизменившийся кадр клиенту не нужен.
                Ok(CaptureOutcome::Timeout) => continue,
                Err(err) if err.is_recoverable() || err.needs_backoff() => {
                    if !wait_for_capture(&mut capturer, &err.to_string())? {
                        break 'session;
                    }
                    recoveries += 1;

                    // Разрешение могло измениться, пока захвата не
                    // было. Верить прежнему нельзя: NVENC не меняет
                    // геометрию на лету, а декодер клиента ждёт SPS
                    // прежней геометрии — то есть смена дала бы не
                    // ошибку, а поток мусора на экране (находка 31).
                    let fresh = capturer.size();
                    if fresh != size {
                        println!(
                            "Разрешение изменилось {}x{} → {}x{}, пересоздаю энкодер",
                            size.width, size.height, fresh.width, fresh.height
                        );
                        size = fresh;
                        encoder = AnyEncoder::new(
                            capturer.device(),
                            EncoderConfig::low_latency(size, 60),
                            epoch,
                        )?;
                    }

                    // Декодер клиента потерял опору, пока захвата не
                    // было: без ключевого он покажет мусор (§5.3).
                    encoder.request_keyframe();
                    continue;
                }
                Err(err) => {
                    println!("Захват сломался необратимо: {err}");
                    break;
                }
            };
            captured += 1;

            if !limiter.should_encode_now() {
                continue;
            }

            let Some(mut encoded) = encoder.encode(frame.texture(), *frame.info())? else {
                continue;
            };
            encoded_frames += 1;

            let keyframe = matches!(encoded.kind, FrameKind::Key);
            if keyframe {
                keyframes += 1;
            }

            match session.send(
                PayloadKind::Video,
                keyframe,
                &encoded.data,
                &mut encoded.info.timings,
            ) {
                Ok(_) => {}
                // Очередь отправки полна — сигнал ДРОПНУТЬ кадр, а не
                // ждать (§5.3). Устаревший кадр не нужен никому.
                Err(bd_transport::TransportError::WouldBlock { .. }) => {
                    let since = now().duration_since(last_keyframe_request);
                    if since >= KEYFRAME_COOLDOWN {
                        last_keyframe_request = now();
                        encoder.request_keyframe();
                    }
                }
                Err(err) => {
                    println!("\nСоединение потеряно: {err}");
                    break;
                }
            }
        }

        // Всё, что клиент зажал, отпускается принудительно: иначе
        // человек за этой машиной останется с залипшими клавишами.
        // Модификаторы уходят последними — отпустить Ctrl раньше C
        // значит напечатать букву вместо копирования.
        if inject_input {
            let release: Vec<_> = tracker.release_events().collect();
            if !release.is_empty() {
                println!("Отпускаю {} зажатых клавиш...", release.len());
                for event in release {
                    let _ = injector.inject(&event);
                }
            }
        }

        report(
            started,
            captured,
            encoded_frames,
            keyframes,
            input_applied,
            input_blocked,
            cursor_updates,
            cursor_shapes,
            recoveries,
            &session,
        );

        Ok(())
    }

    /// Дождаться клиента на сигналинге и пробить NAT ему навстречу.
    ///
    /// Возвращает **пробитый сокет**, который надо отдать QUIC:
    /// отображение в роутере заведено именно для его порта, и
    /// создавать новый значило бы выбросить всю работу (находка 59).
    ///
    /// `None` — пробивать не нужно или не получилось узнать пира. Это
    /// не отказ: без сигналинга работает прямое подключение по адресу,
    /// а в локальной сети пробивание не нужно вовсе.
    ///
    /// # Почему ожидание ограничено
    ///
    /// Хост может простоять сутки, и всё это время `PeerWants` не
    /// придёт. Но держать здесь бесконечное ожидание нельзя: тогда
    /// хост, запущенный «на всякий случай», не принял бы и прямое
    /// подключение по адресу — он висел бы в этой функции.
    ///
    /// Поэтому ждём недолго и, не дождавшись, уходим слушать обычным
    /// способом. Клиент, пришедший позже, всё равно достучится, если
    /// NAT мягкий или его нет; за симметричным — не достучится, и это
    /// записанное ограничение (§5.4).
    fn punch_towards_peer(
        signaling: Option<&Signaling>,
        bind: SocketAddr,
    ) -> Option<std::net::UdpSocket> {
        let signaling = signaling?;

        // Сколько ждать объявления клиента.
        //
        // Полторы минуты: человек, которому продиктовали ID, вводит
        // его за это время с запасом. Дольше — уже не «подключаются
        // прямо сейчас», а «хост стоит и ждёт».
        const PEER_WAIT: Duration = Duration::from_secs(90);

        println!("Жду, кто подключится (пробивание NAT)...");

        let peer: SocketAddr = match signaling.wait(PEER_WAIT) {
            Ok(Some(SignalEvent::PeerWants { id, addr })) => {
                println!("Клиент {id} стучится с {addr} — бью навстречу.");
                match addr.parse() {
                    Ok(a) => a,
                    Err(_) => {
                        // Сервер недоверен (§8.1): он может прислать
                        // что угодно, и до этапа 5 заметить подмену
                        // нечем. Мусор молча не глотаем.
                        println!("⚠  Сигналинг прислал непонятный адрес: {addr}");
                        return None;
                    }
                }
            }
            Ok(Some(other)) => {
                tracing::debug!(?other, "не тот сигнал, пробивать некого");
                return None;
            }
            Ok(None) => {
                println!("Никто не объявился за {} с.", PEER_WAIT.as_secs());
                println!("Продолжаю слушать обычным способом.\n");
                return None;
            }
            Err(e) => {
                println!("⚠  Связь с сигналингом потеряна: {e}");
                return None;
            }
        };

        // Сокет на ТОМ ЖЕ порту, что объявлен серверу: клиент получил
        // именно его и бьёт туда.
        let socket = match std::net::UdpSocket::bind(bind) {
            Ok(s) => s,
            Err(e) => {
                println!("⚠  Не удалось занять {bind}: {e}");
                return None;
            }
        };

        match bd_transport::punch(&socket, peer) {
            Ok(bd_transport::PunchOutcome::Opened) => {
                println!("NAT пробит: встречные пакеты дошли.\n");
            }
            Ok(bd_transport::PunchOutcome::NoAnswer) => {
                println!("Встречных пакетов не было — жду подключения всё равно.");
                println!("(если не выйдет, вероятен symmetric NAT — нужен релей, §5.4)\n");
            }
            Err(e) => {
                println!("⚠  Пробивание не удалось: {e}\n");
            }
        }

        Some(socket)
    }

    /// Дождаться восстановления захвата.
    ///
    /// `Ok(false)` — сдались по таймауту; вызывающий обязан завершить
    /// сессию.
    ///
    /// # Почему любая ошибка ведёт к повтору
    ///
    /// Secure desktop (UAC, Ctrl+Alt+Del, экран блокировки) возвращает
    /// не только `E_ACCESSDENIED`, но и другие коды, полного списка
    /// которых нет. Отличать «временно нельзя» от «сломано навсегда»
    /// по коду ненадёжно, и попытка это делать уже роняла процесс
    /// дважды (находки 32 и 36в).
    ///
    /// Надёжнее время: то, что не восстановилось за минуту,
    /// действительно сломано, а UAC-диалог столько не живёт.
    fn wait_for_capture(capturer: &mut DxgiCapturer, first_error: &str) -> anyhow::Result<bool> {
        let backoff = Duration::from_millis(100);
        let give_up_after = Duration::from_secs(60);
        let mut waited = Duration::ZERO;

        loop {
            match capturer.recover() {
                Ok(()) => {
                    if !waited.is_zero() {
                        println!("Захват восстановлен спустя {:.1} с", waited.as_secs_f64());
                    }
                    return Ok(true);
                }
                Err(e) => {
                    if waited >= give_up_after {
                        println!(
                            "Захват не восстановился за {} с: {e}",
                            give_up_after.as_secs()
                        );
                        return Ok(false);
                    }
                    if waited.is_zero() {
                        println!("Захват недоступен ({first_error}) — жду");
                    }
                    // Повтор без паузы сжёг бы процессор десятками
                    // тысяч вызовов в секунду (находка 32).
                    std::thread::sleep(backoff);
                    waited += backoff;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn report(
        started: std::time::Instant,
        captured: u64,
        encoded_frames: u64,
        keyframes: u64,
        input_applied: u64,
        input_blocked: u64,
        cursor_updates: u64,
        cursor_shapes: u64,
        recoveries: u64,
        session: &QuicTransport,
    ) {
        let elapsed = now().duration_since(started);
        let stats = session.stats();
        let secs = elapsed.as_secs_f64().max(0.001);

        println!("\n=== Итоги сессии ===\n");
        println!("Длительность:     {:.1} с", elapsed.as_secs_f64());
        println!(
            "Захвачено:        {captured} кадров ({:.1}/с)",
            captured as f64 / secs
        );
        println!(
            "Закодировано:     {encoded_frames} ({:.1}/с), из них ключевых {keyframes}",
            encoded_frames as f64 / secs
        );
        println!(
            "Отправлено:       {} кадров, {} датаграмов",
            stats.frames_sent, stats.datagrams_sent
        );
        println!(
            "Битрейт:          {:.1} Мбит/с",
            stats.bitrate(elapsed) / 1_000_000.0
        );
        println!("Курсор:           {cursor_updates} позиций, {cursor_shapes} форм");
        println!("Ввод применён:    {input_applied}, заблокировано UIPI: {input_blocked}");
        println!("Восстановлений:   {recoveries}");
        println!(
            "RTT:              {:.1} мс",
            session.rtt().as_secs_f64() * 1000.0
        );

        // Отброшенные датаграмы означают, что очередь отправки
        // переполнялась: канал не тянет заданный битрейт. Это не
        // ошибка хоста, но об этом надо знать — иначе клиент
        // жалуется на рывки, а причина не видна.
        if stats.datagrams_dropped > 0 {
            println!(
                "\n⚠  Отброшено {} датаграмов: канал не тянет битрейт.",
                stats.datagrams_dropped
            );
            println!("   Адаптация битрейта — этап 6.");
        }
    }

    /// Разобрать `--listen АДРЕС:ПОРТ`.
    fn parse_listen() -> Option<SocketAddr> {
        parse_arg("--listen").and_then(|v| v.parse().ok())
    }

    fn parse_monitor() -> Option<u32> {
        parse_arg("--monitor").and_then(|v| v.parse().ok())
    }

    fn parse_seconds() -> Option<u64> {
        parse_arg("--seconds").and_then(|v| v.parse().ok())
    }

    fn parse_fps() -> Option<u32> {
        parse_arg("--fps").and_then(|v| v.parse().ok())
    }

    fn parse_arg(name: &str) -> Option<String> {
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == name {
                return args.next();
            }
        }
        None
    }

    /// ID этой машины.
    ///
    /// Выводится локально из имени машины и пользователя: устойчив
    /// между запусками, но никем не подтверждён. Настоящий ID выдаёт
    /// сигналинг при регистрации (§5.4) — это придёт вместе с
    /// аккаунтами, пока же сервер принимает тот, что мы назвали.
    fn device_id() -> DeviceId {
        // Общий источник: тот же ID показывает оболочка. Пока каждый
        // считал сам, копии разошлись и машина получила два разных
        // ID (см. DeviceId::of_this_machine).
        DeviceId::of_this_machine()
    }

    fn parse_flag(name: &str) -> bool {
        std::env::args().any(|a| a == name)
    }
}
