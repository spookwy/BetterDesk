//! Клиент BetterDesk: приём, декод, показ, захват ввода.
//!
//! # Чем отличается от пробы `loopback`
//!
//! `loopback` — инструмент замера: он совмещает обе роли в одном
//! процессе и потому вынужден делать вещи, которые продукту не нужны.
//! Главная из них — **захват собственного экрана**. Цикл там общий
//! на обе роли, и приёмник ждал `AcquireNextFrame` с таймаутом 16 мс,
//! хотя захватывать ему нечего.
//!
//! Цена измерена (CLAUDE.md, находка 41): около 20 мс из 21 сидели в
//! стадии `network` на localhost, где сети фактически нет. Кадр
//! приходил вовремя, но забирали его на следующем такте чужого цикла.
//!
//! Здесь захвата нет вовсе. Цикл ждёт **на канале, откуда приходят
//! кадры** (`receive_timeout`), то есть просыпается ровно тогда,
//! когда есть что показать.
//!
//! # Чего клиенту не нужно
//!
//! - захват экрана (DXGI): он показывает чужой;
//! - энкодер: не кодирует ничего (находка 47);
//! - NVENC: и как следствие — работает на машине без NVIDIA.
//!
//! Нужно только D3D11-устройство, на котором живут декодер и окно.
//! Оно берётся через `D3dDevice::for_decode` — без монитора и без
//! дупликации.
//!
//! Запуск: `cargo run --release -p bd-client -- --connect 192.168.1.5:7000`

#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    println!("BetterDesk работает только на Windows (CLAUDE.md §3.1).");
}

#[cfg(windows)]
mod input_bridge;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    run::main()
}

#[cfg(windows)]
mod run {
    use bd_capture::windows::D3dDevice;
    use bd_codec::mediafoundation::D3d11Decoder;
    use bd_codec::{Decoder, DecoderConfig, EncodedFrame, FrameKind};
    use bd_core::clocksync::ClockSync;
    use bd_core::cursor::{CursorPosition, CursorShape};
    use bd_core::frame::{FrameInfo, FrameSize};
    use bd_core::input::SequencedInput;
    use bd_core::metrics::{FrameTimings, LatencyWindow, Stage};
    use bd_core::time::{now, Epoch, Timestamp};
    use bd_input::InputTracker;
    use bd_render::windows::VideoWindow;
    use bd_transport::{PayloadKind, QuicTransport, SignalEvent, Signaling};
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::time::Duration;

    /// Сколько ждать кадра, прежде чем перерисовать окно.
    ///
    /// Ожидание идёт на канале транспорта, а не на таймере: кадр
    /// будит поток сразу по приходе. Таймаут нужен только затем,
    /// чтобы окно жило на статичном экране — хост тогда честно
    /// ничего не шлёт (находка 37), а неперерисованное окно Windows
    /// помечает зависшим.
    const FRAME_WAIT: Duration = Duration::from_millis(16);

    /// Разрешение потока, пока хост не прислал первый кадр.
    ///
    /// Настоящее берётся из декодированного кадра: клиент не знает
    /// заранее, какой экран ему покажут, и угадывать не должен.
    const INITIAL_SIZE: FrameSize = FrameSize {
        width: 1920,
        height: 1080,
    };

    pub fn main() -> anyhow::Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();

        println!("=== BetterDesk — клиент ===\n");

        // Два способа сказать, куда подключаться.
        //
        // `--connect АДРЕС` — прямой, работает без всякого сервера и
        // потому остаётся: в локальной сети адрес известен, и
        // требовать там сигналинг значило бы сделать продукт
        // неработоспособным при недоступном сервере.
        //
        // `--id ЦИФРЫ` — то, ради чего этап 4 и делается: человек
        // вводит девять цифр, адрес выясняется сам.
        let server = match (parse_connect(), parse_id(), parse_arg("--signaling")) {
            // Прямой адрес имеет приоритет: он задан явнее.
            (Some(addr), _, _) => addr,

            (None, Some(id), Some(signaling_url)) => {
                match resolve_by_id(id, &signaling_url) {
                    Ok(addr) => addr,
                    Err(e) => {
                        // Честное сообщение вместо зависания — прямое
                        // требование этапа 4.
                        //
                        // Печатаем причину сами, а наверх отдаём
                        // короткое `Err`: ненулевой код возврата
                        // нужен (по нему судят о прогоне в инструкции
                        // для второй машины), но `anyhow` напечатал бы
                        // полный текст вторым разом с префиксом
                        // `Error:`. Один и тот же абзац дважды
                        // выглядит сбоем программы, а не объяснением.
                        println!("❌ {e}");
                        anyhow::bail!("не удалось найти хост по ID");
                    }
                }
            }

            (None, Some(_), None) => {
                println!("❌ Для подключения по ID нужен адрес сигналинга.");
                println!("   Добавьте --signaling ws://АДРЕС:9000/ws");
                return Ok(());
            }

            (None, None, _) => {
                println!("Запуск — одним из двух способов:\n");
                println!("  По ID (нужен сервер связи):");
                println!("    --id 418207356 --signaling ws://АДРЕС:9000/ws\n");
                println!("  Напрямую по адресу (в локальной сети):");
                println!("    --connect 192.168.1.5:7000\n");
                println!("На хосте при этом:");
                println!("  cargo run --release -p bd-host -- --listen 0.0.0.0:7000");
                println!("  (и то же --signaling, если подключаются по ID)");
                return Ok(());
            }
        };

        // Одна эпоха на весь пайплайн: иначе отметки стадий несравнимы
        // между собой (CLAUDE.md §4.5).
        let epoch = Epoch::new();

        // Устройство БЕЗ монитора и без дупликации.
        //
        // Клиенту нечего захватывать, и требовать от него работающий
        // DXGI-захват значило бы отказывать машинам, где захват
        // недоступен (гибридная графика, §5.1), — хотя показу это
        // не мешает никак.
        let device = match D3dDevice::for_decode() {
            Ok(d) => d,
            Err(err) => {
                println!("❌ D3D11-устройство недоступно: {err}");
                println!();
                println!("Без него нельзя ни декодировать, ни показывать.");
                println!("Сообщите разработчику эту строку.");
                return Err(err.into());
            }
        };

        let mut size = INITIAL_SIZE;

        let mut decoder =
            match D3d11Decoder::new(device.device(), DecoderConfig::low_latency(size), epoch) {
                Ok(d) => d,
                Err(err) => {
                    println!("❌ Аппаратный декодер H.264 недоступен: {err}");
                    println!();
                    println!("Это ограничение машины, а не ошибка сборки.");
                    println!("Декодер берётся встроенный в Windows, и он есть");
                    println!("почти везде — но требует поддержки D3D11 от GPU.");
                    println!();
                    println!("Софтверный декодер (openh264) запланирован");
                    println!("(CLAUDE.md §5.5), но ещё не сделан.");
                    println!();
                    println!("Сообщите разработчику эти строки.");
                    return Err(err.into());
                }
            };
        println!("Декодер: H.264 D3D11VA, выход NV12");

        // Три соединения на трёх портах подряд — так же, как их
        // открывает хост.
        //
        // Сборщик ведёт один `last_delivered` на поток и выбрасывает
        // всё с меньшим номером. Видео идёт 60 кадрами в секунду,
        // ввод — сотнями событий, нумерация у каждого своя: в одной
        // очереди каждый вид считал бы чужие пакеты устаревшими.
        //
        // Три порта — временное решение (см. bd-host): на этапе 4
        // пробивать три порта через NAT дороже, чем один, и тогда это
        // придётся свести к одному соединению с разделением по видам.
        let input_server = SocketAddr::new(server.ip(), server.port() + 1);
        let cursor_server = SocketAddr::new(server.ip(), server.port() + 2);

        println!("Подключаюсь к {server}...");
        let mut video = QuicTransport::connect(server, Duration::from_secs(15), epoch)?;
        println!("Видео: соединение установлено.");
        let mut input = QuicTransport::connect(input_server, Duration::from_secs(15), epoch)?;
        println!("Ввод: подключён.");
        let mut cursor = QuicTransport::connect(cursor_server, Duration::from_secs(15), epoch)?;
        println!("Курсор: подключён.\n");

        let window_size = FrameSize::new(size.width / 2, size.height / 2);
        let mut window = VideoWindow::new(
            device.device(),
            &format!("BetterDesk — {server}"),
            window_size,
            epoch,
        )?;
        println!("Окно: FLIP_DISCARD, latency 1, Present без VSync");
        println!("F9 — статистика. Закройте окно, чтобы выйти.\n");

        // Часы двух машин расходятся на десятки миллисекунд, поэтому
        // метку захвата хоста нельзя вычитать из своего времени
        // напрямую: получится разница часов, а не задержка (находка 40).
        let mut clock_sync = ClockSync::new();

        // Трекер зажатых клавиш. Потеря отпускания необратима: клавиша
        // залипнет на хосте, и человек за ним будет чинить это вслепую
        // (этап 2). Поэтому при выходе всё зажатое отпускается явно.
        let mut tracker = InputTracker::new();
        let mut input_sequence = 0u64;
        // Буфер сообщений окна переиспользуется между итерациями:
        // выделять его заново 60 раз в секунду ни к чему.
        let mut raw_input: Vec<bd_render::windows::RawInputMessage> = Vec::new();
        // Последняя известная позиция мыши. Отпускания кнопок своей
        // позиции не несут, а хосту она нужна, чтобы клик пришёлся
        // туда же, где была рука.
        let mut last_cursor = bd_core::input::MousePosition::new(0.5, 0.5);

        // Ограничение прогона по времени.
        //
        // Без него итоги печатаются только при закрытии окна руками,
        // то есть замер требует присутствия человека — ровно та
        // ситуация, из-за которой критерий «30 минут» висел месяцами
        // (находка 33). С флагом замер между двумя машинами делается
        // одной командой и заканчивается сам.
        let limit = parse_seconds().map(Duration::from_secs);
        if let Some(d) = limit {
            println!("Прогон {} с, затем итоги.\n", d.as_secs());
        }

        let started = now();
        let mut presented = 0u64;
        let mut input_sent = 0u64;
        let mut cursor_applied = 0u64;
        let mut total_times = LatencyWindow::new(4000);
        let mut stage_times: BTreeMap<Stage, LatencyWindow> = BTreeMap::new();
        let mut overlay_lines: Vec<String> = Vec::new();
        let mut last_overlay = now();
        let overlay_period = Duration::from_millis(250);

        // Кадр-контейнер переиспользуется: выделять по буферу на кадр
        // 60 раз в секунду значит мусорить без нужды.
        let mut encoded = EncodedFrame {
            info: FrameInfo::new(
                size,
                bd_core::frame::PixelFormat::Nv12,
                FrameTimings::default(),
            ),
            kind: FrameKind::Delta,
            data: Vec::new(),
        };

        'session: loop {
            if !window.pump_messages() {
                break;
            }

            if limit.is_some_and(|d| now().duration_since(started) >= d) {
                println!("Время прогона вышло.");
                break;
            }

            // ── Ввод: окно → транспорт → хост ──────────────────────
            //
            // Прокачивается каждую итерацию, а не только при наличии
            // кадра: человек, ткнувший мышью, ждёт отклика независимо
            // от того, изменился ли экран.
            {
                window.drain_input(&mut raw_input);
                let (win_w, win_h) = window.client_size();
                let mut to_send: Vec<bd_core::input::InputEvent> = Vec::new();

                for raw in &raw_input {
                    let Some(event) = crate::input_bridge::translate(*raw, win_w, win_h) else {
                        continue;
                    };

                    // Позиция запоминается: отпускания кнопок своей
                    // не несут, а хосту она нужна.
                    if let bd_core::input::InputEvent::MouseMove { position }
                    | bd_core::input::InputEvent::MouseButton { position, .. } = event
                    {
                        last_cursor = position;
                    }

                    // Потеря фокуса разворачивается в конкретные
                    // отпускания: команда `ReleaseAll` сама может
                    // потеряться, а отпускания идемпотентны.
                    if matches!(event, bd_core::input::InputEvent::ReleaseAll) {
                        to_send.extend(
                            tracker
                                .release_events()
                                .map(|e| crate::input_bridge::with_position(e, last_cursor)),
                        );
                        tracker.track(&event);
                    } else {
                        match tracker.track(&event) {
                            bd_input::TrackOutcome::Changed => to_send.push(event),
                            // Автоповтор клавиатуры: хост повторит
                            // сам, гнать это в сеть незачем.
                            bd_input::TrackOutcome::Redundant => {}
                            // Зажато предельное число клавиш.
                            bd_input::TrackOutcome::Overflow => {}
                        }
                    }
                }
                raw_input.clear();

                for event in to_send {
                    let sequenced = SequencedInput {
                        sequence: input_sequence,
                        timestamp: epoch.stamp_now(),
                        event,
                    };
                    input_sequence += 1;

                    let mut timings = FrameTimings::default();
                    if input
                        .send(PayloadKind::Input, false, &sequenced.encode(), &mut timings)
                        .is_ok()
                    {
                        input_sent += 1;
                    }
                }
            }

            // ── Курсор: хост → клиент ─────────────────────────────
            //
            // Цикл, а не одна попытка: позиций приходит несколько за
            // итерацию, и показывать надо последнюю — промежуточные
            // уже устарели.
            loop {
                let mut arrival = FrameTimings::default();
                let delivered = match cursor.receive(&mut arrival) {
                    Ok(Some(d)) => d,
                    Ok(None) => break,
                    Err(_) => break,
                };

                match delivered.kind {
                    // Форма курсора: приходит редко и весит килобайты.
                    PayloadKind::Control => {
                        if let Some(shape) = CursorShape::parse(&delivered.data) {
                            // Без курсора работать можно, без картинки
                            // нельзя: сессию из-за формы не рвём.
                            if window.set_cursor_shape(&shape).is_ok() {
                                cursor_applied += 1;
                            }
                        }
                    }
                    // Позиция: 9 байт, приходит сотни раз в секунду и
                    // обгоняет картинку — иначе стрелка тянулась бы
                    // за рукой человека.
                    _ => {
                        if let Some(position) = CursorPosition::parse(&delivered.data) {
                            window.set_cursor_position(&position);
                        }
                    }
                }
            }

            // ── Видео: ожидание НА КАНАЛЕ, а не на такте цикла ────
            //
            // Здесь и лечится находка 41. Поток паркуется до прихода
            // кадра и просыпается ровно по нему; таймаут нужен лишь
            // затем, чтобы перерисовать окно на статичном экране.
            let mut arrival = FrameTimings::default();
            let delivered = match video.receive_timeout(FRAME_WAIT, &mut arrival) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    // Кадра нет: хост ничего не прислал. Окно обязано
                    // жить, иначе Windows пометит его зависшим
                    // (находка 37).
                    window.redraw_last_frame()?;
                    continue;
                }
                Err(err) => {
                    println!("\nСоединение потеряно: {err}");
                    break 'session;
                }
            };

            // Тайминги берутся из пришедшего кадра, а не из локальных:
            // иначе задержка мерилась бы по пути, которого кадр не
            // проходил (находка 27).
            encoded.info.timings = arrival;
            encoded.kind = if delivered.keyframe {
                FrameKind::Key
            } else {
                FrameKind::Delta
            };

            if delivered.captured_at_micros != 0 {
                let network_delay =
                    clock_sync.observe(delivered.captured_at_micros, epoch.stamp_now());

                // Отметка `Captured` восстанавливается в НАШЕЙ шкале:
                // момент получения минус измеренная задержка. Так весь
                // расчёт задержки работает без изменений, а разница
                // часов в него не попадает.
                if clock_sync.is_settled() {
                    let captured_local =
                        epoch.stamp_now().as_micros().saturating_sub(network_delay);
                    encoded
                        .info
                        .timings
                        .mark(Stage::Captured, Timestamp::from_micros(captured_local));
                }
            }

            encoded.data = delivered.data;

            let decoded = match decoder.decode(&encoded) {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(err) if err.needs_keyframe() => {
                    // Потеря синхронизации лечится ключевым кадром от
                    // хоста, а не пересозданием декодера (§5.2).
                    // Просить нечем: обратного канала запросов пока
                    // нет, хост шлёт ключевые по своим потерям.
                    continue;
                }
                Err(err) => {
                    println!("Ошибка декодирования: {err}");
                    break;
                }
            };

            // Разрешение потока меняется, когда хост переключает
            // монитор или меняет разрешение. Верить своим настройкам
            // нельзя: настоящий размер приходит из выходного типа
            // декодера (находка 18).
            let actual = decoded.info().size;
            if actual != size && !actual.is_empty() {
                println!(
                    "Разрешение потока: {}x{} → {}x{}",
                    size.width, size.height, actual.width, actual.height
                );
                size = actual;
            }

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

            // Оверлей пересчитывается 4 раза в секунду: при 60 fps
            // цифры менялись бы быстрее, чем глаз читает.
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

                for (stage, w) in &stage_times {
                    if let Some(med) = w.median() {
                        overlay_lines.push(format!(
                            "  {:<7} {:>5.2} мс",
                            stage.label(),
                            med.as_secs_f64() * 1000.0
                        ));
                    }
                }

                let stats = video.stats();
                overlay_lines.push(format!(
                    "RTT      {:>7.1} мс",
                    video.rtt().as_secs_f64() * 1000.0
                ));
                // Потери кадров и датаграмов печатаются отдельно: они
                // расходятся в разы, потому что потеря одного датаграма
                // убивает весь кадр (находка 28).
                overlay_lines.push(format!(
                    "потери   {:>5} кадров / {} датаграмов",
                    stats.frames_lost, stats.datagrams_dropped
                ));
            }
        }

        // Всё зажатое отпускается принудительно: иначе человек за
        // хостом остался бы с зажатым Ctrl и чинил это вслепую.
        // Модификаторы уходят последними — отпустить Ctrl раньше C
        // значит дать хосту напечатать букву вместо копирования.
        let release: Vec<_> = tracker
            .release_events()
            .map(|e| crate::input_bridge::with_position(e, last_cursor))
            .collect();
        if !release.is_empty() {
            println!("Отпускаю {} зажатых клавиш на хосте...", release.len());
            for event in release {
                let sequenced = SequencedInput {
                    sequence: input_sequence,
                    timestamp: epoch.stamp_now(),
                    event,
                };
                input_sequence += 1;
                let mut timings = FrameTimings::default();
                let _ = input.send(PayloadKind::Input, false, &sequenced.encode(), &mut timings);
            }
            // Датаграмы уходят из своего потока: без паузы процесс
            // завершится раньше, чем поток QUIC успеет их отправить.
            std::thread::sleep(Duration::from_millis(200));
        }

        report(
            started,
            presented,
            input_sent,
            cursor_applied,
            &total_times,
            &stage_times,
            &video,
            server.ip().is_loopback(),
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn report(
        started: std::time::Instant,
        presented: u64,
        input_sent: u64,
        cursor_applied: u64,
        total_times: &LatencyWindow,
        stage_times: &BTreeMap<Stage, LatencyWindow>,
        video: &QuicTransport,
        // Хост на этом же компьютере: вердикт по критерию LAN тогда
        // выносить нельзя (см. ниже).
        local: bool,
    ) {
        let elapsed = now().duration_since(started);
        let stats = video.stats();

        println!("\n=== Итоги сессии ===\n");
        println!("Длительность:     {:.1} с", elapsed.as_secs_f64());
        println!(
            "Показано кадров:  {presented} ({:.1} fps)",
            presented as f64 / elapsed.as_secs_f64().max(0.001)
        );
        println!("Событий ввода:    {input_sent} отправлено");
        println!("Курсор:           {cursor_applied} форм применено");
        println!(
            "Потери:           {} кадров, {} датаграмов",
            stats.frames_lost, stats.datagrams_dropped
        );
        println!(
            "RTT:              {:.1} мс",
            video.rtt().as_secs_f64() * 1000.0
        );

        match (total_times.median(), total_times.p95()) {
            (Some(med), Some(p95)) => {
                println!(
                    "\nЗадержка glass-to-glass: медиана {:.2} мс, p95 {:.2} мс",
                    med.as_secs_f64() * 1000.0,
                    p95.as_secs_f64() * 1000.0
                );

                println!("\nПо стадиям (медиана):");
                for (stage, w) in stage_times {
                    if let Some(m) = w.median() {
                        println!(
                            "  {:<10} {:>6.2} мс",
                            stage.label(),
                            m.as_secs_f64() * 1000.0
                        );
                    }
                }

                // Критерий этапа 3 (docs/roadmap.md): ≤ 35 мс в LAN
                // между двумя РЕАЛЬНЫМИ машинами.
                //
                // # Почему вердикт зависит от того, где хост
                //
                // Порог 35 мс задан для двух машин в локальной сети.
                // Вынести по нему вердикт на прогоне, где обе стороны
                // на одном компьютере, значит объявить критерий
                // выполненным, не проверив то, что он требует: там нет
                // ни настоящего RTT, ни расхождения часов, ни коммутатора.
                //
                // Это ровно ошибка находки 29, где проба сравнивала
                // измеренное не с тем порогом и выглядела убедительно,
                // будучи неправой. Поэтому на localhost печатается
                // цифра и прямая оговорка, а галочка — нет.
                const LAN_TARGET_MS: f64 = 35.0;
                let med_ms = med.as_secs_f64() * 1000.0;
                println!();

                if local {
                    println!("Хост на этой же машине — это замер ПАЙПЛАЙНА, не сети.");
                    println!("Критерий этапа 3 (≤ {LAN_TARGET_MS} мс) им НЕ проверяется:");
                    println!("он требует двух машин в локальной сети.");
                } else if med_ms <= LAN_TARGET_MS {
                    println!("✅ {med_ms:.2} мс ≤ {LAN_TARGET_MS} мс — критерий этапа 3 выполнен");
                    println!("   (порог для LAN; через интернет он другой — этап 4)");
                } else {
                    println!(
                        "❌ {med_ms:.2} мс > {LAN_TARGET_MS} мс — критерий этапа 3 НЕ выполнен"
                    );
                    println!("   (порог для LAN; через интернет он другой — этап 4)");
                }
            }
            _ => {
                // Пустой замер — это не «ноль миллисекунд». Без часов,
                // сведённых с хостом, задержку считать нельзя, и
                // молчаливый ноль был бы хуже отсутствия цифры.
                println!("\nЗадержка не измерена: кадров с меткой времени не пришло.");
                println!("Причина обычно одна — хост старой версии протокола.");
            }
        }
    }

    /// Узнать адрес хоста по его девятизначному ID.
    ///
    /// # Почему ошибки здесь такие подробные
    ///
    /// Это единственное место продукта, где человек может ошибиться
    /// молча: опечататься в ID, обратиться к выключенной машине,
    /// указать не тот сервер. Все три случая выглядят одинаково —
    /// «не подключается», — и без внятного различения человек будет
    /// винить программу.
    ///
    /// Критерий этапа 4 требует ровно этого: «отказ соединения даёт
    /// понятное сообщение, а не зависание».
    fn resolve_by_id(
        id: bd_core::device::DeviceId,
        signaling_url: &str,
    ) -> anyhow::Result<SocketAddr> {
        println!("Ищу {id} через {signaling_url}...");

        let signaling = Signaling::connect(signaling_url, Duration::from_secs(10))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Свой адрес хосту нужен, чтобы бить встречно при пробивании
        // NAT. Порт 0 — «любой свободный»: настоящий выберет ОС при
        // соединении, и сейчас это значение лишь заполняет поле.
        //
        // Оговорка, важная для этапа с ICE: пробивание NAT требует,
        // чтобы это был ТОТ ЖЕ порт, с которого пойдёт QUIC. Сейчас
        // это не так, и потому hole punching здесь ещё не работает —
        // в одной сети он и не нужен, а через интернет потребует
        // связать сокет сигналинга с сокетом транспорта.
        let own = "0.0.0.0:0".parse().expect("константа");
        signaling
            .connect_to(id, own)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Ждём ответа. 15 секунд с запасом: сервер отвечает мгновенно,
        // и всё, что дольше, означает потерю связи с ним.
        match signaling.wait(Duration::from_secs(15)) {
            Ok(Some(SignalEvent::PeerFound { addr, .. })) => {
                println!("Найден: {addr}");
                addr.parse().map_err(|_| {
                    // Сервер недоверен (§8.1): он может прислать
                    // что угодно, и до этапа 5 заметить подмену
                    // нечем. Мусор здесь — не наша ошибка, но и
                    // молча его глотать нельзя.
                    anyhow::anyhow!("сигналинг прислал непонятный адрес: {addr}")
                })
            }
            Ok(Some(SignalEvent::Failed { reason })) => {
                anyhow::bail!(
                    "{reason}\n   \
                     Проверьте: тот ли ID, запущен ли хост, \
                     и указан ли у него тот же --signaling"
                )
            }
            Ok(Some(other)) => {
                anyhow::bail!("неожиданный ответ сигналинга: {other:?}")
            }
            Ok(None) => {
                anyhow::bail!(
                    "сигналинг не ответил за 15 с.\n   \
                     Сервер запущен, но молчит — возможно, перегружен"
                )
            }
            Err(e) => anyhow::bail!("связь с сигналингом потеряна: {e}"),
        }
    }

    /// Разобрать `--connect АДРЕС:ПОРТ`.
    fn parse_connect() -> Option<SocketAddr> {
        parse_arg("--connect").and_then(|v| v.parse().ok())
    }

    /// Разобрать `--id 418207356`.
    ///
    /// Принимает то, что человек копирует из переписки: с пробелами,
    /// дефисами, точками.
    fn parse_id() -> Option<bd_core::device::DeviceId> {
        parse_arg("--id").and_then(|v| bd_core::device::DeviceId::parse(&v))
    }

    /// Разобрать `--seconds N` — ограничение прогона по времени.
    fn parse_seconds() -> Option<u64> {
        parse_arg("--seconds").and_then(|v| v.parse().ok())
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
}
