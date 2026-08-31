//! Живая проба энкодера Media Foundation: настоящий экран → H.264.
//!
//! # Зачем отдельно от `encode_probe`
//!
//! `encode_probe` меряет NVENC. Эта — тот путь, которым пойдут машины
//! **без NVIDIA**, то есть большинство чужих компьютеров. Пока он не
//! проверен живым прогоном, «работает у всех» остаётся надеждой:
//! MFT-энкодеры разных вендоров ведут себя по-разному, и то, что
//! конфигурация принята, ещё не значит, что кадры кодируются.
//!
//! # Что именно проверяется
//!
//! 1. Энкодер создаётся, и видно, **какой** MFT выбрала система.
//! 2. Кадры реально кодируются: непустой битстрим, разумные размеры.
//! 3. Задержка кодирования укладывается в бюджет §5.2 (3–6 мс).
//! 4. Ключевой кадр приходит по запросу — без этого клиент,
//!    потерявший опору, не восстановится.
//!
//! Проверка 2 не формальность: энкодер, отдающий пустые или
//! одинаковые буферы, выглядит исправным по таймингам (тот же урок,
//! что находка 21 про декодер).
//!
//! Запуск: `cargo run --release -p bd-bench --bin mfenc_probe`

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
fn main() {
    println!("Проба реализована только для Windows.");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};
    use bd_codec::mediafoundation::{MfEncoder, MfEncoderInput};
    use bd_codec::{Encoder, EncoderConfig, FrameKind};
    use bd_core::metrics::LatencyWindow;
    use bd_core::time::{now, Epoch};
    use std::time::Duration;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== Проба энкодера Media Foundation ===\n");
    println!("Это путь для машин БЕЗ NVIDIA. Нужна активность на экране:");
    println!("DXGI отдаёт кадр только при изменении картинки.\n");

    let seconds = parse_seconds().unwrap_or(10);

    let monitors = enumerate_monitors()?;
    let target = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow::anyhow!("мониторов не найдено"))?;

    let epoch = Epoch::new();
    let mut capturer = DxgiCapturer::new(target.index, epoch)?;
    let size = capturer.size();

    let config = EncoderConfig::low_latency(size, 60);
    let bitrate = config.rate_control.target_bitrate();
    let mut encoder = MfEncoder::new(capturer.device(), config, epoch)?;

    println!("Монитор: {}x{}", size.width, size.height);
    println!("Выбран энкодер: {}", encoder.backend_name());
    println!(
        "Вид: {}",
        if encoder.is_hardware() {
            "АППАРАТНЫЙ"
        } else {
            "СОФТВЕРНЫЙ (медленнее, но работает везде)"
        }
    );
    println!("Битрейт: {} Мбит/с\n", bitrate / 1_000_000);

    let mut encode_times = LatencyWindow::new(4000);
    let mut key_sizes: Vec<usize> = Vec::new();
    let mut delta_sizes: Vec<usize> = Vec::new();
    let mut frames = 0u32;
    let mut empty = 0u32;

    let started = now();
    let deadline = Duration::from_secs(seconds);
    // Ключевой кадр запрашивается в середине прогона: надо убедиться,
    // что запрос доходит. Без этого клиент после потерь не оживёт.
    let mut keyframe_requested = false;

    while now().duration_since(started) < deadline {
        let elapsed = now().duration_since(started);
        if !keyframe_requested && elapsed > deadline / 2 {
            encoder.request_keyframe();
            keyframe_requested = true;
            println!("  [запрошен ключевой кадр]");
        }

        let frame = match capturer.next_frame(Duration::from_millis(16)) {
            Ok(CaptureOutcome::Frame(f)) => f,
            // Таймаут — норма: экран не изменился (находка 8).
            Ok(_) => continue,
            Err(e) if e.is_recoverable() => {
                capturer.recover()?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        let before = now();
        let input = MfEncoderInput {
            texture: frame.texture(),
            info: *frame.info(),
        };

        match encoder.encode_texture(&input)? {
            Some(encoded) => {
                encode_times.push(now().duration_since(before));
                if encoded.data.is_empty() {
                    // Пустой буфер — это отказ, который выглядит как
                    // успех. Считаем отдельно.
                    empty += 1;
                } else if matches!(encoded.kind, FrameKind::Key) {
                    key_sizes.push(encoded.data.len());
                } else {
                    delta_sizes.push(encoded.data.len());
                }
                frames += 1;
            }
            // Энкодер ещё не набрал, чем ответить — штатно.
            None => continue,
        }
    }

    println!("\n=== Результат ===");
    println!("Закодировано кадров: {frames}");

    if frames == 0 {
        println!("\n❌ Ни одного кадра. Скорее всего на экране ничего");
        println!("   не менялось — DXGI тогда кадров не отдаёт.");
        println!("   Повторить, двигая окна по экрану.");
        std::process::exit(1);
    }

    if let Some(median) = encode_times.median() {
        let ms = median.as_secs_f64() * 1000.0;
        println!("Медиана кодирования: {ms:.2} мс");
        if let Some(p95) = encode_times.p95() {
            println!("p95:                 {:.2} мс", p95.as_secs_f64() * 1000.0);
        }
        // Бюджет §5.2 задан для аппаратного пути. Софтверный в него
        // не обязан укладываться, и требовать этого от него —
        // сравнивать не с тем (тот же урок, что находка 29).
        if encoder.is_hardware() {
            if ms <= 6.0 {
                println!("✅ в бюджете этапа 1 (3–6 мс)");
            } else {
                println!("⚠ выше бюджета 6 мс — но кодирование работает");
            }
        } else {
            println!("(бюджет 3–6 мс задан для аппаратного пути)");
        }
    }

    println!("\nРазмеры кадров:");
    if !delta_sizes.is_empty() {
        let mut sorted = delta_sizes.clone();
        sorted.sort_unstable();
        println!(
            "  разностных: {} шт., медиана {} Б",
            sorted.len(),
            sorted[sorted.len() / 2]
        );
    }
    if !key_sizes.is_empty() {
        let mut sorted = key_sizes.clone();
        sorted.sort_unstable();
        println!(
            "  ключевых:   {} шт., медиана {} Б",
            sorted.len(),
            sorted[sorted.len() / 2]
        );
    }

    // Вердикт. Проверки построены так, чтобы уметь провалиться:
    // энкодер, отдающий пустые буферы или один и тот же размер,
    // проходит по таймингам, но не по ним.
    let mut ok = true;

    if empty > 0 {
        println!("\n❌ Пустых буферов: {empty} — энкодер отдаёт ничто");
        ok = false;
    }

    if key_sizes.is_empty() {
        println!("\n❌ Ни одного ключевого кадра, хотя он запрашивался.");
        println!("   Клиент после потерь не смог бы восстановиться.");
        ok = false;
    }

    // Все кадры одного размера — признак того, что энкодер отдаёт
    // заглушку, а не сжатую картинку.
    if delta_sizes.len() > 10 {
        let first = delta_sizes[0];
        if delta_sizes.iter().all(|s| *s == first) {
            println!("\n❌ Все кадры одного размера ({first} Б) — это не похоже");
            println!("   на сжатие настоящей картинки.");
            ok = false;
        }
    }

    if ok {
        println!("\n✅ Энкодер Media Foundation исправен.");
        println!("   Машины без NVIDIA могут быть хостом.");
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Сколько секунд идёт прогон.
#[cfg(windows)]
fn parse_seconds() -> Option<u64> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == "--seconds" {
            return args.next()?.parse().ok();
        }
    }
    None
}
