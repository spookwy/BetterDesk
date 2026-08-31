//! Проба захвата экрана: работает ли DXGI на этой машине и с какой задержкой.
//!
//! Отвечает на вопрос, критичный для этапа 1 (CLAUDE.md §0.1): на ноутбуке
//! с гибридной графикой DXGI Desktop Duplication может быть недоступен,
//! потому что требует исполнения на адаптере дисплея. Проба перечисляет
//! мониторы, пробует захват на каждом и печатает результат.
//!
//! Запуск: `cargo run --release -p bd-bench --bin capture_probe`

#![forbid(unsafe_code)]

use bd_core::metrics::LatencyWindow;
use bd_core::time::{now, Epoch};
use std::time::Duration;

#[cfg(not(windows))]
fn main() {
    println!("Проба захвата реализована только для Windows.");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== Проба захвата экрана ===\n");

    let monitors = enumerate_monitors()?;
    if monitors.is_empty() {
        println!("Мониторы не найдены. DXGI недоступен.");
        return Ok(());
    }

    println!("Найдено мониторов: {}", monitors.len());
    for m in &monitors {
        let primary = if m.is_primary {
            " (основной)"
        } else {
            ""
        };
        println!(
            "  [{}] {} — {}x{}{}",
            m.index, m.device_name, m.size.width, m.size.height, primary
        );
    }
    println!();

    // Захватываем с основного монитора, иначе с первого доступного.
    let target = monitors
        .iter()
        .find(|m| m.is_primary)
        .unwrap_or(&monitors[0]);

    println!("Пробуем захват с монитора {}...\n", target.index);

    let epoch = Epoch::new();
    let mut capturer = match DxgiCapturer::new(target.index, epoch) {
        Ok(c) => c,
        Err(err) => {
            println!("DXGI-захват НЕ РАБОТАЕТ: {err}");
            println!();
            if err.is_recoverable() {
                println!("Ошибка восстановимая — возможно, дупликацию занял");
                println!("другой процесс (OBS, другой стример).");
            } else {
                println!("Вероятная причина — гибридная графика: DXGI требует,");
                println!("чтобы процесс исполнялся на адаптере дисплея.");
                println!("Это тот случай, ради которого нужен WGC-фоллбэк");
                println!("(CLAUDE.md §5.1).");
            }
            return Ok(());
        }
    };

    println!("DXGI-захват РАБОТАЕТ.\n");

    // Собираем статистику за несколько секунд реальной работы.
    const PROBE_DURATION: Duration = Duration::from_secs(5);
    const FRAME_TIMEOUT: Duration = Duration::from_millis(16);

    let started = now();
    let mut acquire_times = LatencyWindow::new(1000);
    let mut frames = 0u32;
    let mut timeouts = 0u32;
    let mut unchanged = 0u32;
    let mut recoveries = 0u32;

    println!(
        "Замер {} секунд — подвигайте окнами для нагрузки...",
        PROBE_DURATION.as_secs()
    );

    while now().duration_since(started) < PROBE_DURATION {
        let before = now();
        match capturer.next_frame(FRAME_TIMEOUT) {
            Ok(CaptureOutcome::Frame(frame)) => {
                acquire_times.push(now().duration_since(before));
                frames += 1;
                if !frame.info().content_changed {
                    unchanged += 1;
                }
            }
            Ok(CaptureOutcome::Timeout) => {
                // Экран не изменился — штатная ситуация, не ошибка.
                timeouts += 1;
            }
            Err(err) if err.is_recoverable() => {
                // Смена разрешения, UAC, блокировка экрана. Пересоздаём.
                recoveries += 1;
                if let Err(e) = capturer.recover() {
                    println!("\nВосстановление не удалось: {e}");
                    break;
                }
            }
            Err(err) => {
                println!("\nНевосстановимая ошибка: {err}");
                break;
            }
        }
    }

    let elapsed = now().duration_since(started);

    if frames == 0 {
        println!("\n=== Результат ===");
        println!("Кадров не получено: за всё время экран ни разу не изменился");
        println!("({timeouts} таймаутов). Это корректная работа API, а не сбой:");
        println!("DXGI отдаёт кадр только при изменении картинки.");
        println!();
        println!("Чтобы получить осмысленные цифры, запустите пробу и в это");
        println!("время подвигайте окно или проиграйте видео.");
        return Ok(());
    }

    println!("\n=== Результат ===");
    println!("Время замера:        {:.1} с", elapsed.as_secs_f64());
    println!("Кадров получено:     {frames}");
    println!("  из них без правок: {unchanged} (изменился только курсор)");
    println!("Таймаутов:           {timeouts} (экран не менялся)");
    println!("Восстановлений:      {recoveries}");
    println!(
        "Частота кадров:      {:.1} fps",
        frames as f64 / elapsed.as_secs_f64()
    );

    if let (Some(med), Some(p95), Some(max)) = (
        acquire_times.median(),
        acquire_times.p95(),
        acquire_times.max(),
    ) {
        println!();
        println!("Время AcquireNextFrame:");
        println!("  медиана: {:.2} мс", med.as_secs_f64() * 1000.0);
        println!("  p95:     {:.2} мс", p95.as_secs_f64() * 1000.0);
        println!("  максимум:{:.2} мс", max.as_secs_f64() * 1000.0);
        println!();
        println!("Бюджет на захват — 1-2 мс (docs/latency-budget.md §1.1).");
        println!("Учтите: сюда входит ожидание кадра, поэтому при простое");
        println!("экрана значения будут близки к таймауту в 16 мс.");
    }

    let size = capturer.size();
    println!();
    println!("Разрешение источника: {}x{}", size.width, size.height);

    Ok(())
}
