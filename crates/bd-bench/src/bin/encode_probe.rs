//! Живой тест: захват экрана → аппаратный энкод, с замером задержки.
//!
//! Первый настоящий кусок пайплайна из этапа 1. Проверяет то, что
//! нельзя проверить юнит-тестами:
//!
//! - реальные кадры экрана доходят до NVENC и кодируются;
//! - zero-copy работает (кадр не проходит через системную память);
//! - сколько миллисекунд занимает энкод — бюджет 3–6 мс
//!   (docs/latency-budget.md §1.1);
//! - какой битрейт получается на реальном содержимом экрана.
//!
//! Запуск: `cargo run --release -p bd-bench --bin encode_probe`
//! Дополнительно: `-- --seconds 20` для более длинного замера.

// Проба работает с сырыми указателями D3D11-текстур, поэтому unsafe
// здесь неизбежен. Это инструмент замера, а не часть продукта.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(windows, nvenc_available)))]
fn main() {
    println!("Проба требует Windows и собранного бэкенда NVENC.");
    println!("См. vendor/nvcodec/README.md.");
}

#[cfg(all(windows, nvenc_available))]
fn main() -> anyhow::Result<()> {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};
    use bd_codec::nvenc::{EncoderInput, NvencEncoder};
    use bd_codec::{Encoder, EncoderConfig, FrameKind};
    use bd_core::metrics::LatencyWindow;
    use bd_core::time::{now, Epoch};
    use std::time::Duration;
    use windows::core::Interface;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let seconds = parse_seconds().unwrap_or(10);

    println!("=== Живой тест: захват → энкод ===\n");

    let monitors = enumerate_monitors()?;
    let target = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow::anyhow!("мониторы не найдены"))?;

    println!(
        "Монитор {}: {} — {}x{}",
        target.index, target.device_name, target.size.width, target.size.height
    );

    // Общая эпоха для захвата и энкода: только так тайминги кадра
    // сравнимы между стадиями (CLAUDE.md §4.5).
    let epoch = Epoch::new();
    let mut capturer = DxgiCapturer::new(target.index, epoch)?;
    let size = capturer.size();

    // Энкодер работает на устройстве захвата — иначе текстуру пришлось
    // бы копировать между адаптерами, и zero-copy потерялся бы.
    let device = capturer.device_ptr();
    let config = EncoderConfig::low_latency(size, 60);
    println!(
        "Энкодер: H.264 {}x{}@60, CBR {} Мбит/с\n",
        size.width,
        size.height,
        config.rate_control.target_bitrate() / 1_000_000
    );

    // SAFETY: устройство принадлежит `capturer`, который жив до конца
    // функции и уничтожается после энкодера (объявлен раньше по тексту,
    // значит дропается позже).
    let mut encoder = unsafe { NvencEncoder::new(device, config, epoch) }?;

    println!("Замер {seconds} с. Подвигайте окна или включите видео.\n");

    let started = now();
    let deadline = Duration::from_secs(seconds);
    let frame_timeout = Duration::from_millis(16);

    let mut encode_times = LatencyWindow::new(2000);
    let mut total_times = LatencyWindow::new(2000);
    let mut frames = 0u32;
    let mut skipped = 0u32;
    let mut keyframes = 0u32;
    let mut bytes = 0u64;
    let mut recoveries = 0u32;

    while now().duration_since(started) < deadline {
        match capturer.next_frame(frame_timeout) {
            Ok(CaptureOutcome::Frame(frame)) => {
                // Кадр, где изменился только курсор, перекодировать
                // незачем: экономия такта энкодера и битрейта (§5.1).
                if !frame.info().content_changed {
                    skipped += 1;
                    continue;
                }

                let input = EncoderInput {
                    texture: frame.texture().as_raw(),
                    info: *frame.info(),
                };

                let before_encode = now();
                match encoder.encode(&input) {
                    Ok(Some(encoded)) => {
                        encode_times.push(now().duration_since(before_encode));
                        frames += 1;
                        bytes += encoded.data.len() as u64;
                        if encoded.kind == FrameKind::Key {
                            keyframes += 1;
                        }
                        // Задержка от захвата до конца энкода — то, что
                        // мы контролируем на этом шаге пайплайна.
                        if let Some(span) = encoded.info.timings.span(
                            bd_core::metrics::Stage::Captured,
                            bd_core::metrics::Stage::Encoded,
                        ) {
                            total_times.push(span);
                        }
                    }
                    Ok(None) => {
                        // Энкодер придержал кадр. При наших настройках
                        // (без B-кадров) быть не должно.
                        skipped += 1;
                    }
                    Err(err) => {
                        println!("Ошибка кодирования: {err}");
                        break;
                    }
                }
            }
            Ok(CaptureOutcome::Timeout) => {}
            Err(err) if err.is_recoverable() => {
                recoveries += 1;
                if let Err(e) = capturer.recover() {
                    println!("Восстановление не удалось: {e}");
                    break;
                }
            }
            Err(err) => {
                println!("Ошибка захвата: {err}");
                break;
            }
        }
    }

    let elapsed = now().duration_since(started);

    println!("=== Результат ===");
    if frames == 0 {
        println!("Кадров не закодировано: экран не менялся.");
        println!("Повторите, создав движение на экране.");
        return Ok(());
    }

    let secs = elapsed.as_secs_f64();
    println!("Время замера:      {secs:.1} с");
    println!(
        "Кадров:            {frames} ({:.1} fps)",
        frames as f64 / secs
    );
    println!("  ключевых:        {keyframes}");
    println!("Пропущено:         {skipped} (менялся только курсор)");
    println!("Восстановлений:    {recoveries}");
    println!(
        "Средний битрейт:   {:.1} Мбит/с",
        bytes as f64 * 8.0 / secs / 1_000_000.0
    );
    println!(
        "Средний размер:    {:.1} КБ/кадр",
        bytes as f64 / frames as f64 / 1024.0
    );

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;

    if let (Some(med), Some(p95), Some(max)) = (
        encode_times.median(),
        encode_times.p95(),
        encode_times.max(),
    ) {
        println!();
        println!("Время энкода (бюджет 3–6 мс):");
        println!("  медиана: {:.2} мс", ms(med));
        println!("  p95:     {:.2} мс", ms(p95));
        println!("  максимум:{:.2} мс", ms(max));

        let verdict = if ms(med) <= 6.0 {
            "В БЮДЖЕТЕ"
        } else {
            "ВЫШЕ БЮДЖЕТА — разбираться до следующего шага"
        };
        println!("  вердикт: {verdict}");
    }

    if let (Some(med), Some(p95)) = (total_times.median(), total_times.p95()) {
        println!();
        println!("Захват → энкод суммарно:");
        println!("  медиана: {:.2} мс", ms(med));
        println!("  p95:     {:.2} мс", ms(p95));
        println!();
        println!("Это ещё не glass-to-glass: нет сети, декода и рендера.");
        println!("Полный бюджет — docs/latency-budget.md.");
    }

    Ok(())
}

/// Разобрать `--seconds N` из аргументов.
#[cfg(all(windows, nvenc_available))]
fn parse_seconds() -> Option<u64> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--seconds" {
            return args.next()?.parse().ok();
        }
    }
    None
}
