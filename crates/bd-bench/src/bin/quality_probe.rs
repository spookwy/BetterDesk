//! Диагностика всплесков качества: размеры кадров и ключевые кадры.
//!
//! # Зачем
//!
//! Пользователь наблюдает периодическую пикселизацию на ~0.5 с при
//! в целом стабильной картинке. Гипотеза — полные IDR-кадры: при CBR
//! и VBV размером в один кадр (§5.2) большой ключевой кадр не
//! помещается в буфер, и энкодер резко роняет качество, чтобы уложиться
//! в битрейт.
//!
//! Гипотезу надо **измерить**, а не чинить вслепую: если всплески
//! окажутся не на ключевых кадрах, причина другая, и правка
//! энкодера ничего не даст.
//!
//! Проба печатает: распределение размеров кадров, позиции ключевых
//! кадров и всплесков, а также их совпадение. Совпадение и есть
//! проверяемое утверждение.
//!
//! Запуск: `cargo run --release -p bd-bench --bin quality_probe`

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(windows, nvenc_available)))]
fn main() {
    println!("Проба требует Windows и собранного бэкенда NVENC.");
}

#[cfg(all(windows, nvenc_available))]
fn main() -> anyhow::Result<()> {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};
    use bd_codec::nvenc::{EncoderInput, NvencEncoder};
    use bd_codec::{Encoder, EncoderConfig, FrameKind};
    use bd_core::time::{now, Epoch};
    use std::time::Duration;
    use windows::core::Interface;

    let seconds: u64 = parse_arg("--seconds").unwrap_or(30);

    println!("=== Диагностика всплесков качества ===\n");

    let monitors = enumerate_monitors()?;
    let index = parse_arg::<u32>("--monitor").unwrap_or_else(|| {
        monitors
            .iter()
            .find(|m| m.is_primary)
            .map_or(0, |m| m.index)
    });

    let epoch = Epoch::new();
    let mut capturer = DxgiCapturer::new(index, epoch)?;
    let size = capturer.size();
    let config = EncoderConfig::low_latency(size, 60);
    let bitrate = config.rate_control.target_bitrate();

    println!("Монитор {index}: {}x{}", size.width, size.height);
    println!("Битрейт: {} Мбит/с", bitrate / 1_000_000);
    // Целевой размер кадра при заданном битрейте — та величина,
    // относительно которой всплеск имеет смысл измерять.
    let budget = bitrate as f64 / 60.0 / 8.0;
    println!("Бюджет кадра при 60 fps: {:.0} Б\n", budget);

    // SAFETY: устройство принадлежит `capturer`, объявленному раньше
    // энкодера, поэтому переживает его.
    let mut encoder = unsafe { NvencEncoder::new(capturer.device_ptr(), config, epoch) }?;

    // Ограничение частоты кодирования. Без него DXGI отдаёт кадры
    // так быстро, как меняется экран, и все они идут в энкодер —
    // а битрейт рассчитан на 60 кадров в секунду. Лишние кадры
    // делят тот же битрейт на большее число, то есть отнимают биты
    // у каждого кадра.
    let cap_fps: u32 = parse_arg("--cap-fps").unwrap_or(0);
    if cap_fps > 0 {
        println!("Ограничение частоты кодирования: {cap_fps} кадров/с");
    } else {
        println!("Частота кодирования НЕ ограничена (--cap-fps 60 чтобы ограничить)");
    }

    println!("Замер {seconds} с. Нужна активность на экране.\n");

    struct Sample {
        bytes: usize,
        keyframe: bool,
        at: Duration,
    }

    let started = now();
    let mut samples: Vec<Sample> = Vec::new();
    let min_interval = if cap_fps > 0 {
        Duration::from_secs_f64(1.0 / cap_fps as f64)
    } else {
        Duration::ZERO
    };
    let mut last_encoded: Option<bd_core::time::Instant> = None;

    while now().duration_since(started) < Duration::from_secs(seconds) {
        let frame = match capturer.next_frame(Duration::from_millis(16)) {
            Ok(CaptureOutcome::Frame(f)) => f,
            Ok(CaptureOutcome::Timeout) => continue,
            Err(err) if err.is_recoverable() => {
                capturer.recover()?;
                continue;
            }
            Err(err) => {
                println!("Ошибка захвата: {err}");
                break;
            }
        };

        if !frame.info().content_changed {
            continue;
        }

        // Кадр приходит раньше, чем позволяет заданная частота, —
        // пропускаем. Кодировать его значило бы отнять биты у тех
        // кадров, что реально увидит пользователь.
        if !min_interval.is_zero() {
            if let Some(last) = last_encoded {
                if now().duration_since(last) < min_interval {
                    continue;
                }
            }
            last_encoded = Some(now());
        }

        let input = EncoderInput {
            texture: frame.texture().as_raw(),
            info: *frame.info(),
        };

        let Some(encoded) = encoder.encode(&input)? else {
            continue;
        };

        samples.push(Sample {
            bytes: encoded.data.len(),
            keyframe: matches!(encoded.kind, FrameKind::Key),
            at: now().duration_since(started),
        });
    }

    if samples.is_empty() {
        println!("Ни одного кадра. Нужна активность на экране.");
        return Ok(());
    }

    let mut sorted: Vec<usize> = samples.iter().map(|s| s.bytes).collect();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let p95 = sorted[sorted.len() * 95 / 100];
    let max = *sorted.last().unwrap_or(&0);

    let elapsed = now().duration_since(started).as_secs_f64().max(0.001);
    let actual_fps = samples.len() as f64 / elapsed;
    let total_bytes: usize = samples.iter().map(|s| s.bytes).sum();
    let actual_bitrate = total_bytes as f64 * 8.0 / elapsed;

    println!("=== Размеры кадров ===");
    println!("Кадров:   {}", samples.len());
    println!("медиана:  {median} Б");
    println!("p95:      {p95} Б");
    println!(
        "максимум: {max} Б  ({:.1}× медианы)",
        max as f64 / median.max(1) as f64
    );

    println!("\n=== Частота и битрейт ===");
    println!("Кодируется:      {actual_fps:.1} кадров/с");
    println!("Энкодер настроен: 60 кадров/с");
    println!("Фактический битрейт: {:.1} Мбит/с", actual_bitrate / 1e6);
    println!("Заданный битрейт:    {:.1} Мбит/с", bitrate as f64 / 1e6);

    // Главная проверка этой пробы. Битрейт делится на фактическое
    // число кадров, а не на заданное: лишние кадры отнимают биты
    // у каждого следующего.
    if actual_fps > 70.0 {
        let share = bitrate as f64 / actual_fps / 8.0;
        println!(
            "\n  ⚠ Кодируется {:.1}× больше кадров, чем заложено.",
            actual_fps / 60.0
        );
        println!("  На кадр приходится {share:.0} Б вместо {budget:.0} Б —");
        println!("  энкодер вынужден резать качество, чтобы уложиться в CBR.");
    }

    // Периоды голодания: подряд идущие кадры, упирающиеся в потолок
    // VBV. Именно они и выглядят как пикселизация — энкодеру не
    // хватает бит, и он огрубляет картинку.
    //
    // Признак — кадр близок к бюджету VBV (> 80 %): значит, энкодер
    // отдал всё, что мог, и качество ограничено не сценой, а битрейтом.
    let vbv_budget = bitrate as f64 / 60.0 / 8.0;
    let starving = |b: usize| b as f64 > vbv_budget * 0.8;

    let mut runs: Vec<(Duration, usize)> = Vec::new();
    let mut run_start: Option<Duration> = None;
    let mut run_len = 0usize;
    for s in &samples {
        if starving(s.bytes) {
            if run_start.is_none() {
                run_start = Some(s.at);
            }
            run_len += 1;
        } else {
            if let Some(start) = run_start.take() {
                // Серия от трёх кадров — уже заметна глазу.
                if run_len >= 3 {
                    runs.push((start, run_len));
                }
            }
            run_len = 0;
        }
    }
    if let Some(start) = run_start {
        if run_len >= 3 {
            runs.push((start, run_len));
        }
    }

    println!("\n=== Периоды голодания битрейта ===");
    println!("(кадры подряд, упирающиеся в потолок VBV — это и есть");
    println!(" то, что видно как пикселизация)");
    println!("Найдено серий (≥3 кадров): {}", runs.len());
    for (at, len) in runs.iter().take(15) {
        println!("  {:>7.2} с   {len} кадров подряд", at.as_secs_f64());
    }
    if runs.len() > 15 {
        println!("  ... и ещё {}", runs.len() - 15);
    }

    let keyframes: Vec<&Sample> = samples.iter().filter(|s| s.keyframe).collect();
    println!("\n=== Ключевые кадры ===");
    println!("Всего: {} из {}", keyframes.len(), samples.len());

    if !keyframes.is_empty() {
        let key_bytes: usize = keyframes.iter().map(|s| s.bytes).sum::<usize>() / keyframes.len();
        println!("Средний размер ключевого: {key_bytes} Б");
        println!(
            "  это {:.1}× медианы обычного кадра",
            key_bytes as f64 / median.max(1) as f64
        );

        // Интервалы между ключевыми кадрами: регулярность укажет на
        // периодические IDR, разброс — на реакцию по требованию.
        if keyframes.len() > 1 {
            let mut gaps: Vec<f64> = Vec::new();
            for pair in keyframes.windows(2) {
                gaps.push((pair[1].at - pair[0].at).as_secs_f64());
            }
            let avg: f64 = gaps.iter().sum::<f64>() / gaps.len() as f64;
            let min = gaps.iter().cloned().fold(f64::MAX, f64::min);
            let maxg = gaps.iter().cloned().fold(0.0, f64::max);
            println!("Интервал между ключевыми: сред {avg:.2} с, мин {min:.2}, макс {maxg:.2}");
        }

        println!("\nПервые 12 ключевых кадров (время, размер):");
        for s in keyframes.iter().take(12) {
            println!(
                "  {:>7.2} с   {:>8} Б  ({:.1}× бюджета кадра)",
                s.at.as_secs_f64(),
                s.bytes,
                s.bytes as f64 / budget
            );
        }
    }

    // Всплеск — кадр заметно крупнее обычного. Порог в 3 медианы
    // отделяет реальные пики от естественного разброса.
    let threshold = median * 3;
    let spikes: Vec<&Sample> = samples.iter().filter(|s| s.bytes > threshold).collect();

    println!("\n=== Всплески размера (> 3× медианы = {threshold} Б) ===");
    println!("Всего всплесков: {}", spikes.len());

    if !spikes.is_empty() {
        let on_key = spikes.iter().filter(|s| s.keyframe).count();
        println!(
            "Из них на ключевых кадрах: {on_key} ({:.0} %)",
            on_key as f64 / spikes.len() as f64 * 100.0
        );

        println!("\nПервые 12 всплесков:");
        for s in spikes.iter().take(12) {
            println!(
                "  {:>7.2} с   {:>8} Б   {}",
                s.at.as_secs_f64(),
                s.bytes,
                if s.keyframe {
                    "КЛЮЧЕВОЙ"
                } else {
                    "обычный"
                }
            );
        }

        // Распределение всплесков по секундам: периодическая
        // пикселизация «раз в пару секунд» обязана быть здесь видна
        // как регулярный узор. Ровный фон означает, что всплески
        // следуют за содержимым экрана, а не за таймером энкодера.
        println!("\n=== Всплески по секундам ===");
        let mut per_second = vec![0u32; seconds as usize + 1];
        for s in &spikes {
            let idx = s.at.as_secs() as usize;
            if idx < per_second.len() {
                per_second[idx] += 1;
            }
        }
        let peak = per_second.iter().copied().max().unwrap_or(1).max(1);
        for (sec, count) in per_second.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let bar = "#".repeat((*count as usize * 40 / peak as usize).max(1));
            println!("  {sec:>3} с | {bar} {count}");
        }

        println!("\n=== Вывод ===");
        if on_key * 100 / spikes.len().max(1) >= 80 {
            println!("Всплески идут на ключевых кадрах — гипотеза про IDR");
            println!("подтверждается. Лечится Intra Refresh вместо полных IDR");
            println!("(CLAUDE.md §5.2): обновление размазывается по кадрам,");
            println!("и пика битрейта не возникает.");
        } else {
            println!("Всплески НЕ привязаны к ключевым кадрам — причина");
            println!("другая. Смотреть на сцену: резкая смена содержимого");
            println!("экрана даёт крупные кадры законно.");
        }
    } else {
        println!("Всплесков нет. Причина пикселизации не в размере кадров —");
        println!("искать в другом месте (потери в транспорте, VBV, рендер).");
    }

    Ok(())
}

/// Значение именованного аргумента.
#[cfg(all(windows, nvenc_available))]
fn parse_arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next()?.parse().ok();
        }
    }
    None
}
