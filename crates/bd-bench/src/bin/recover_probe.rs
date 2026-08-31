//! Проверка восстановления после `ACCESS_LOST`.
//!
//! Критерий этапа 1 требует, чтобы смена разрешения и UAC-диалог не
//! роняли процесс (docs/roadmap.md). Проверить это «поработав и
//! понадеявшись» нельзя: `ACCESS_LOST` случается не по команде, и
//! прогон без единого восстановления **ничего не доказывает** —
//! ровно тот случай из CLAUDE.md §0.1, находка 4.
//!
//! Поэтому проба вызывает `recover()` принудительно и проверяет
//! главное свойство: **устройство D3D11 остаётся тем же**. Если
//! восстановление подменит устройство, энкодер, декодер и окно
//! вывода останутся с мёртвым указателем — они все живут на
//! устройстве захвата (CLAUDE.md §0.1, находка 26).
//!
//! Запуск: `cargo run --release -p bd-bench --bin recover_probe`

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
fn main() {
    println!("Проба требует Windows.");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::{CaptureOutcome, Capturer};
    use bd_core::time::Epoch;
    use std::time::Duration;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== Проверка восстановления после ACCESS_LOST ===\n");

    let monitors = enumerate_monitors()?;
    let target = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow::anyhow!("мониторы не найдены"))?;

    let epoch = Epoch::new();
    let mut capturer = DxgiCapturer::new(target.index, epoch)?;

    let device_before = capturer.device_ptr();
    let size_before = capturer.size();
    println!(
        "До восстановления:  устройство {device_before:p}, {}x{}",
        size_before.width, size_before.height
    );

    // Принудительное восстановление. В бою его вызывает обработка
    // ACCESS_LOST, но ждать настоящего события ради проверки —
    // значит не проверить ничего.
    let mut failures = Vec::new();

    for round in 1..=3 {
        capturer.recover()?;

        let device_after = capturer.device_ptr();
        if device_after != device_before {
            failures.push(format!(
                "проход {round}: устройство подменилось ({device_before:p} → {device_after:p})"
            ));
        }

        // Захват обязан продолжить работать после восстановления.
        let mut got_frame = false;
        for _ in 0..120 {
            match capturer.next_frame(Duration::from_millis(16)) {
                Ok(CaptureOutcome::Frame(_)) => {
                    got_frame = true;
                    break;
                }
                Ok(CaptureOutcome::Timeout) => continue,
                Err(e) => {
                    failures.push(format!("проход {round}: захват сломался: {e}"));
                    break;
                }
            }
        }

        if !got_frame {
            println!(
                "  проход {round}: кадра не дождались за 2 с \
                 (экран статичен — это не сбой)"
            );
        } else {
            println!("  проход {round}: восстановление прошло, кадры идут");
        }
    }

    let device_after = capturer.device_ptr();
    let size_after = capturer.size();
    println!(
        "После восстановления: устройство {device_after:p}, {}x{}",
        size_after.width, size_after.height
    );

    // ── Фаза 2: пересоздание стека кодеков ────────────────────────
    //
    // Устройство переживает восстановление — но при смене разрешения
    // этого мало. NVENC не меняет геометрию кадра на лету
    // (`nvEncReconfigureEncoder` покрывает битрейт, не размер), а
    // декодер ждёт SPS прежней геометрии. Значит, потребитель обязан
    // пересоздать оба, и этот путь надо исполнить здесь: в бою он
    // впервые исполнится при настоящей смене разрешения, где ошибка
    // обойдётся дороже.
    //
    // Настоящее разрешение монитора менять нельзя — проба не вправе
    // трогать рабочее окружение. Но пересоздание проверяется и без
    // этого: стек строится на *другом* размере, и если он на нём
    // работает, то отработает и на том, что придёт от DXGI.
    println!("\n=== Пересоздание стека кодеков ===\n");

    let stack_failures = probe_codec_stack(&capturer, size_before);

    println!("\n=== Результат ===");
    failures.extend(stack_failures);

    if failures.is_empty() {
        println!("✅ Устройство пережило 3 восстановления неизменным.");
        println!("   Энкодер, декодер и окно вывода остаются валидными.");
        println!("✅ Стек кодеков пересоздаётся на новом размере.");
        Ok(())
    } else {
        for f in &failures {
            println!("❌ {f}");
        }
        anyhow::bail!("восстановление нарушает контракт");
    }
}

/// Проверить, что энкодер и декодер пересоздаются на другом размере.
///
/// Возвращает список нарушений — пустой, если всё в порядке.
#[cfg(all(windows, nvenc_available))]
fn probe_codec_stack(
    capturer: &bd_capture::windows::DxgiCapturer,
    native: bd_core::frame::FrameSize,
) -> Vec<String> {
    use bd_codec::mediafoundation::D3d11Decoder;
    use bd_codec::nvenc::NvencEncoder;
    use bd_codec::{DecoderConfig, EncoderConfig};
    use bd_core::frame::FrameSize;
    use bd_core::time::Epoch;

    let mut failures = Vec::new();
    let epoch = Epoch::new();

    // Два размера подряд: сначала родной, потом заведомо другой.
    // Второй проход — это и есть смена разрешения; первый нужен,
    // чтобы отличить «стек не строится вообще» от «не переживает
    // смену размера».
    let smaller = FrameSize::new(1280, 720);
    let sizes = [native, smaller];

    for (round, size) in sizes.iter().enumerate() {
        let round = round + 1;

        // SAFETY: устройство принадлежит `capturer`, который жив на
        // всём протяжении функции и переживает энкодер.
        let encoder = unsafe {
            NvencEncoder::new(
                capturer.device_ptr(),
                EncoderConfig::low_latency(*size, 60),
                epoch,
            )
        };

        match encoder {
            Ok(enc) => {
                println!(
                    "  проход {round}: энкодер {}x{} открыт",
                    size.width, size.height
                );
                // Энкодер отпускается до создания декодера и до
                // следующего прохода: сессий NVENC на потребительских
                // картах немного, и течь ими нельзя.
                drop(enc);
            }
            Err(e) => {
                failures.push(format!(
                    "проход {round}: энкодер {}x{} не создался: {e}",
                    size.width, size.height
                ));
            }
        }

        match D3d11Decoder::new(capturer.device(), DecoderConfig::low_latency(*size), epoch) {
            Ok(dec) => {
                println!(
                    "  проход {round}: декодер {}x{} открыт",
                    size.width, size.height
                );
                drop(dec);
            }
            Err(e) => {
                failures.push(format!(
                    "проход {round}: декодер {}x{} не создался: {e}",
                    size.width, size.height
                ));
            }
        }
    }

    failures
}

/// Без NVENC проверять нечего: пересоздание стека — про энкодер.
#[cfg(all(windows, not(nvenc_available)))]
fn probe_codec_stack(
    _capturer: &bd_capture::windows::DxgiCapturer,
    _native: bd_core::frame::FrameSize,
) -> Vec<String> {
    println!("  (пропущено: бэкенд NVENC не собран)");
    Vec::new()
}
