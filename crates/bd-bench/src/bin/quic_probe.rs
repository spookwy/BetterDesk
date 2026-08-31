//! Проверка QUIC-транспорта: настоящее соединение через сокет.
//!
//! # Зачем отдельная проба
//!
//! Юнит-тесты проверяют конфигурацию, но не соединение: рукопожатие
//! требует сокетов, портов и двух сторон. Ошибки вроде несогласованного
//! ALPN или неверного имени в сертификате видны **только** здесь —
//! конфигурация при этом собирается без единой жалобы.
//!
//! Проба поднимает хост и клиента в одном процессе и гоняет между
//! ними данные через настоящий UDP на `127.0.0.1`. Это уже сеть:
//! ядро, сокеты, MTU, рукопожатие TLS — всё настоящее, кроме
//! расстояния.
//!
//! Запуск: `cargo run --release -p bd-bench --bin quic_probe`
//! Между машинами:
//!   хост:   `... --bin quic_probe -- --host 0.0.0.0:7000`
//!   клиент: `... --bin quic_probe -- --connect 192.168.1.5:7000`

#![forbid(unsafe_code)]

use bd_core::metrics::FrameTimings;
use bd_core::time::{now, Epoch};
use bd_transport::{PayloadKind, QuicTransport};
use std::net::SocketAddr;
use std::time::Duration;

/// Сколько кадров прогнать в проверке пропускной способности.
const FRAMES: usize = 300;

/// Размер кадра: близко к настоящему разностному кадру 1080p.
const FRAME_BYTES: usize = 8_000;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== Проверка QUIC-транспорта ===\n");

    if let Some(addr) = parse_arg("--host") {
        return run_host(addr);
    }
    if let Some(addr) = parse_arg("--connect") {
        return run_client(addr);
    }

    run_loopback()
}

/// Хост в отдельном процессе: ждёт клиента и отвечает.
fn run_host(bind: SocketAddr) -> anyhow::Result<()> {
    println!("Хост слушает {bind}, жду клиента (60 с)...\n");

    let epoch = Epoch::new();
    let mut transport = QuicTransport::host(bind, Duration::from_secs(60), epoch)?;

    println!("Клиент подключился. Принимаю кадры.\n");

    let started = now();
    let mut received = 0u64;

    while now().duration_since(started) < Duration::from_secs(60) {
        let mut timings = FrameTimings::default();
        match transport.receive(&mut timings) {
            Ok(Some(frame)) => {
                received += 1;
                if received.is_multiple_of(50) {
                    println!(
                        "  принято {received}, номер {}, {} Б, RTT {:.1} мс",
                        frame.sequence,
                        frame.data.len(),
                        transport.rtt().as_secs_f64() * 1000.0
                    );
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_micros(200)),
            Err(e) if e.is_recoverable() => continue,
            Err(e) => {
                println!("Соединение завершено: {e}");
                break;
            }
        }
    }

    let stats = transport.stats();
    println!("\n=== Результат хоста ===");
    println!("Принято кадров: {received}");
    println!("Потеряно кадров: {}", stats.frames_lost);
    Ok(())
}

/// Клиент в отдельном процессе: подключается и шлёт.
fn run_client(server: SocketAddr) -> anyhow::Result<()> {
    println!("Подключаюсь к {server}...\n");

    let epoch = Epoch::new();
    let mut transport = QuicTransport::connect(server, Duration::from_secs(15), epoch)?;

    println!(
        "Соединение установлено, RTT {:.1} мс\n",
        transport.rtt().as_secs_f64() * 1000.0
    );

    let payload = vec![0xABu8; FRAME_BYTES];
    let mut sent = 0u64;
    let mut refused = 0u64;

    for i in 0..FRAMES {
        let mut timings = FrameTimings::default();
        match transport.send(PayloadKind::Video, i == 0, &payload, &mut timings) {
            Ok(_seq) => sent += 1,
            // Очередь полна — сеть не успевает. Кадр отбрасывается,
            // и это правильное поведение (§5.3), а не ошибка.
            Err(e) if e.is_recoverable() => refused += 1,
            Err(e) => {
                println!("Отправка прервана: {e}");
                break;
            }
        }
        // Темп 60 кадров/с, как у настоящего потока.
        std::thread::sleep(Duration::from_millis(16));
    }

    // Дать последним кадрам уйти до закрытия.
    std::thread::sleep(Duration::from_millis(500));

    let stats = transport.stats();
    println!("\n=== Результат клиента ===");
    println!("Отправлено кадров: {sent}, отброшено очередью: {refused}");
    println!("Датаграмов: {}", stats.datagrams_sent);
    println!("RTT: {:.1} мс", transport.rtt().as_secs_f64() * 1000.0);
    Ok(())
}

/// Обе стороны в одном процессе через настоящий сокет.
fn run_loopback() -> anyhow::Result<()> {
    println!("Хост и клиент в одном процессе, настоящий UDP на 127.0.0.1.\n");

    let epoch = Epoch::new();

    // Хост поднимается в отдельном потоке: его конструктор блокируется
    // до подключения клиента, а клиента ещё нет.
    //
    // Порт узнаём заранее: `QuicTransport::host` его не возвращает,
    // поэтому берём фиксированный и проверяем, что он свободен.
    let port = pick_free_port()?;
    let host_addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;

    let host_thread = std::thread::spawn(move || -> anyhow::Result<(u64, u64)> {
        let mut transport = QuicTransport::host(host_addr, Duration::from_secs(20), epoch)?;
        let started = now();
        let mut received = 0u64;
        let mut corrupted = 0u64;

        // Ждём либо все кадры, либо разумный таймаут: часть кадров
        // может потеряться, и вечное ожидание превратило бы пробу
        // в зависание.
        while received < FRAMES as u64 && now().duration_since(started) < Duration::from_secs(20) {
            let mut timings = FrameTimings::default();
            match transport.receive(&mut timings) {
                Ok(Some(frame)) => {
                    received += 1;
                    // Сверка целостности: испорченный поток декодер
                    // чаще проглотит, чем отвергнет (находка 21).
                    if frame.data.len() != FRAME_BYTES || !frame.data.iter().all(|&b| b == 0xAB) {
                        corrupted += 1;
                    }
                }
                Ok(None) => std::thread::sleep(Duration::from_micros(200)),
                Err(e) if e.is_recoverable() => continue,
                Err(_) => break,
            }
        }

        Ok((received, corrupted))
    });

    // Клиенту нужно, чтобы хост успел встать на сокет.
    std::thread::sleep(Duration::from_millis(300));

    let mut client = QuicTransport::connect(host_addr, Duration::from_secs(10), epoch)?;
    println!("✅ Рукопожатие прошло.\n");

    let payload = vec![0xABu8; FRAME_BYTES];
    let mut sent = 0u64;
    let mut refused = 0u64;
    let send_started = now();

    for i in 0..FRAMES {
        let mut timings = FrameTimings::default();
        match client.send(PayloadKind::Video, i == 0, &payload, &mut timings) {
            Ok(_seq) => sent += 1,
            Err(e) if e.is_recoverable() => refused += 1,
            Err(e) => {
                println!("Отправка прервана: {e}");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let send_elapsed = now().duration_since(send_started);
    let (received, corrupted) = host_thread
        .join()
        .map_err(|_| anyhow::anyhow!("поток хоста упал"))??;

    let stats = client.stats();

    println!("=== Результат ===");
    println!("Отправлено кадров:  {sent} (отброшено очередью: {refused})");
    println!("Принято кадров:     {received}");
    println!("Искажено:           {corrupted}");
    println!("Датаграмов:         {}", stats.datagrams_sent);
    println!(
        "Фрагментов на кадр: {:.1}",
        stats.datagrams_sent as f64 / stats.frames_sent.max(1) as f64
    );
    println!(
        "RTT:                {:.2} мс",
        client.rtt().as_secs_f64() * 1000.0
    );
    println!(
        "Пропускная способность: {:.1} Мбит/с",
        (sent as f64 * FRAME_BYTES as f64 * 8.0) / send_elapsed.as_secs_f64() / 1e6
    );

    let mut failures = Vec::new();
    if received == 0 {
        failures.push("ни один кадр не дошёл — путь не проверен".to_string());
    }
    if corrupted > 0 {
        failures.push(format!("транспорт исказил {corrupted} кадров"));
    }
    // На localhost потерь быть не должно: сеть идеальна, а всё, что
    // теряется, теряем мы сами.
    let delivery = received as f64 / sent.max(1) as f64;
    if delivery < 0.95 {
        failures.push(format!(
            "на localhost дошло лишь {:.0} % кадров — теряем сами",
            delivery * 100.0
        ));
    }

    println!();
    if !failures.is_empty() {
        for f in &failures {
            println!("❌ {f}");
        }
        anyhow::bail!("проверка QUIC провалена")
    }
    println!("✅ QUIC-транспорт работает: рукопожатие, датаграмы, сборка.");

    // Транспорт первой фазы закрывается ЯВНО, до начала второй.
    //
    // Иначе он доживает до конца функции, а его хост-поток уже
    // завершился — соединение висит без собеседника и через 8 с
    // idle-таймаута закрывается, роняя вторую фазу вместе с собой.
    // Диагноз выглядел бы как «вид теряется», хотя маршрутизация
    // ни при чём (тот же класс, что находка 39).
    drop(client);

    println!();
    run_mixed_kinds()?;

    Ok(())
}

/// Фаза 2: три вида нагрузки по ОДНОМУ соединению.
///
/// # Зачем это отдельной фазой на живом соединении
///
/// До этапа 4 видео, ввод и курсор шли тремя соединениями на трёх
/// портах (находка 56), и вид нагрузки в заголовке ни на что не
/// влиял — в каждом канале был ровно один вид. Теперь вид стал
/// **адресом**: по нему приёмник выбирает, в какую сборку положить
/// фрагмент.
///
/// Юнит-тесты это покрывают, но покрывают в памяти. Здесь тот же
/// класс риска, что в находке 38: конфигурации сторон по отдельности
/// валидны, а вместе не работают, и увидеть это можно только на
/// настоящем соединении. Вдобавок ошибка была бы **тихой** — не отказ,
/// а «курсор не двигается», причём искать стали бы в коде курсора.
fn run_mixed_kinds() -> anyhow::Result<()> {
    println!("=== Фаза 2: видео, ввод и курсор одним соединением ===\n");

    /// Сколько пакетов каждого вида прогнать.
    const EACH: usize = 60;

    let epoch = Epoch::new();
    let port = pick_free_port()?;
    let host_addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;

    let host_thread = std::thread::spawn(move || -> anyhow::Result<[u64; 3]> {
        let mut transport = QuicTransport::host(host_addr, Duration::from_secs(20), epoch)?;
        let started = now();
        // Счётчики по видам: видео, ввод, курсор.
        let mut got = [0u64; 3];

        while got.iter().sum::<u64>() < (EACH * 3) as u64
            && now().duration_since(started) < Duration::from_secs(20)
        {
            let mut timings = FrameTimings::default();
            match transport.receive(&mut timings) {
                Ok(Some(frame)) => {
                    // Байты помечены видом: перепутанная маршрутизация
                    // дала бы не потерю, а чужое содержимое, и без
                    // этой сверки выглядела бы как успех (находка 21).
                    let expected = match frame.kind {
                        PayloadKind::Video => Some((0usize, 0x11)),
                        PayloadKind::Input => Some((1usize, 0x22)),
                        PayloadKind::Cursor => Some((2usize, 0x33)),
                        _ => None,
                    };
                    if let Some((index, byte)) = expected {
                        if frame.data.iter().all(|&b| b == byte) {
                            got[index] += 1;
                        }
                    }
                }
                // Спим ОЧЕНЬ коротко: приёмная очередь держит четыре
                // кадра (`RECV_QUEUE_FRAMES`), и всё, что не забрали
                // вовремя, поток сети отбрасывает — правильное
                // поведение (§5.3), но здесь оно выглядело бы потерей
                // вида. Первая версия спала 200 мкс и теряла больше
                // половины пакетов именно так.
                Ok(None) => std::thread::yield_now(),
                Err(e) if e.is_recoverable() => continue,
                Err(_) => break,
            }
        }

        Ok(got)
    });

    std::thread::sleep(Duration::from_millis(300));
    let mut client = QuicTransport::connect(host_addr, Duration::from_secs(10), epoch)?;

    // Три вида вперемешку, как их шлёт настоящий хост: видео крупное
    // и редкое, курсор мелкий и частый.
    let kinds = [
        (PayloadKind::Video, 0x11u8, 4_000usize),
        (PayloadKind::Input, 0x22, 25),
        (PayloadKind::Cursor, 0x33, 9),
    ];
    let mut sent = [0u64; 3];

    for _ in 0..EACH {
        for (index, (kind, byte, len)) in kinds.iter().enumerate() {
            let payload = vec![*byte; *len];
            let mut timings = FrameTimings::default();
            match client.send(*kind, false, &payload, &mut timings) {
                Ok(_) => sent[index] += 1,
                Err(e) if e.is_recoverable() => {}
                Err(e) => {
                    // Отправка оборвалась — это про канал, а не про
                    // маршрутизацию. Печатаем причину: без неё
                    // «дошло меньше отправленного» выглядит как потеря
                    // вида, и чинить пошли бы не то (находка 39).
                    println!("⚠  Отправка прервана на {kind:?}: {e}");
                    break;
                }
            }
            // Пауза ВНУТРИ пачки, а не только между ними.
            //
            // Настоящий хост не шлёт три вида одним залпом: кадр
            // уходит раз в 16 мс, курсор — между кадрами. Залп из
            // трёх пакетов подряд переполняет приёмную очередь, и
            // проба меряла бы не маршрутизацию, а глубину очереди
            // (находка 29 — вердикт не о том, что записано).
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    let got = host_thread
        .join()
        .map_err(|_| anyhow::anyhow!("поток хоста упал"))??;

    let names = ["видео", "ввод", "курсор"];
    let mut failures = Vec::new();
    for i in 0..3 {
        println!(
            "{:8} отправлено {:3}, дошло {:3}",
            names[i], sent[i], got[i]
        );
        // # Что этот порог проверяет и чего не проверяет
        //
        // Проба гоняет пакеты плотнее настоящего потока, а приёмная
        // очередь держит четыре кадра (`RECV_QUEUE_FRAMES`): часть
        // пакетов поток сети отбрасывает, и это ПРАВИЛЬНОЕ поведение
        // (§5.3), а не поломка. Требовать здесь стопроцентной
        // доставки значило бы выносить вердикт о глубине очереди под
        // видом вердикта о маршрутизации (находка 29).
        //
        // Проверяется ровно то, ради чего фаза написана: что вид не
        // пропадает ЦЕЛИКОМ и не отстаёт от остальных в разы. Ошибка
        // маршрутизации выглядит именно так — один вид почти не
        // доходит, пока другие идут нормально.
        if got[i] * 4 < sent[i] * 3 {
            failures.push(format!(
                "{}: дошло {} из {} — вид теряется, а не задерживается",
                names[i], got[i], sent[i]
            ));
        }
    }

    // Перекос между видами — второй признак ошибки маршрутизации, и
    // более чувствительный, чем абсолютный порог: очередь отбрасывает
    // всех примерно поровну, а перепутанный вид проседает один.
    let best = got.iter().copied().max().unwrap_or(0);
    let worst = got.iter().copied().min().unwrap_or(0);
    if best > 0 && worst * 2 < best {
        failures.push(format!(
            "перекос между видами: лучший {best}, худший {worst} — \
             очередь роняет всех поровну, один вид проседает только \
             при ошибке маршрутизации"
        ));
    }

    println!();
    if failures.is_empty() {
        println!("✅ Три вида идут одним соединением и не мешают друг другу.");
        Ok(())
    } else {
        for f in &failures {
            println!("❌ {f}");
        }
        anyhow::bail!("сведение каналов в одно соединение сломано")
    }
}

/// Найти свободный порт.
///
/// Привязываемся и сразу отпускаем: между этим и стартом хоста порт
/// теоретически могут занять, но на практике вероятность ничтожна,
/// а альтернатива — возвращать порт из `QuicTransport::host`, что
/// усложнило бы его API ради одной пробы.
fn pick_free_port() -> anyhow::Result<u16> {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    Ok(socket.local_addr()?.port())
}

/// Значение именованного аргумента.
fn parse_arg(name: &str) -> Option<SocketAddr> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next()?.parse().ok();
        }
    }
    None
}
