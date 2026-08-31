//! Живой тест: захват → энкод → декод, с замером задержки.
//!
//! Продолжение `encode_probe` на один шаг вперёд. Проверяет то, что
//! юнит-тестами не проверяется никак:
//!
//! - MFT действительно принимает наш поток NVENC (а не «должен бы»);
//! - декод идёт аппаратно и кадр остаётся в GPU — если бы он вернулся
//!   в системной памяти, декодер сказал бы об этом ошибкой;
//! - сколько миллисекунд занимает декод — бюджет 3–5 мс
//!   (docs/latency-budget.md);
//! - захват + энкод + декод суммарно, то есть весь пайплайн, кроме
//!   сети и рендера.
//!
//! Запуск: `cargo run --release -p bd-bench --bin decode_probe`
//! Дополнительно: `-- --seconds 20`.

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
    use bd_codec::mediafoundation::D3d11Decoder;
    use bd_codec::nvenc::{EncoderInput, NvencEncoder};
    use bd_codec::{Decoder, DecoderConfig, Encoder, EncoderConfig};
    use bd_core::metrics::{LatencyWindow, Stage};
    use bd_core::time::{now, Epoch};
    use std::time::Duration;
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Query, D3D11_QUERY_DESC, D3D11_QUERY_EVENT};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let seconds = parse_seconds().unwrap_or(10);

    println!("=== Живой тест: захват → энкод → декод ===\n");

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

    // Общая эпоха для всех стадий: только так тайминги кадра
    // сравнимы между собой (CLAUDE.md §4.5).
    let epoch = Epoch::new();
    let mut capturer = DxgiCapturer::new(target.index, epoch)?;
    let size = capturer.size();

    let device = capturer.device_ptr();
    let encoder_config = EncoderConfig::low_latency(size, 60);
    println!(
        "Энкодер: H.264 {}x{}@60, CBR {} Мбит/с",
        size.width,
        size.height,
        encoder_config.rate_control.target_bitrate() / 1_000_000
    );

    // SAFETY: устройство принадлежит `capturer`, который жив до конца
    // функции и объявлен раньше энкодера — значит, дропается позже.
    let mut encoder = unsafe { NvencEncoder::new(device, encoder_config, epoch) }?;

    // Декодер работает на том же устройстве. В продукте он будет на
    // машине клиента и на её собственном устройстве; здесь общее —
    // это проба пайплайна, а не эмуляция сети.
    let mut decoder =
        D3d11Decoder::new(capturer.device(), DecoderConfig::low_latency(size), epoch)?;
    println!("Декодер: H.264 D3D11VA, выход NV12\n");

    // Query для честного замера: ProcessOutput возвращает текстуру,
    // в которую GPU может ещё писать. Без ожидания завершения мы
    // измерили бы только диспетчеризацию вызова, а не декод.
    // Ожидание — диагностический приём: в продукте синхронизация
    // с GPU в горячем пути была бы прямой добавкой к задержке.
    // Клон — это AddRef на COM-интерфейс, а не копия устройства:
    // иначе ссылка на капчурер жила бы весь цикл и мешала бы вызывать
    // его изменяемые методы.
    let device = capturer.device().clone();
    // SAFETY: устройство живо. В windows 0.62 контекст возвращается
    // значением, а не через out-параметр, как в MSDN (CLAUDE.md §0.1,
    // находка 6).
    let context = unsafe { device.GetImmediateContext() }?;

    let query_desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query: Option<ID3D11Query> = None;
    // SAFETY: устройство живо; описание и выходной параметр — живые
    // локальные переменные.
    unsafe { device.CreateQuery(&query_desc, Some(&mut query)) }?;
    let query = query.ok_or_else(|| anyhow::anyhow!("не удалось создать query"))?;

    println!("Замер {seconds} с. Подвигайте окна или включите видео.\n");

    let started = now();
    let deadline = Duration::from_secs(seconds);
    let frame_timeout = Duration::from_millis(16);

    let mut decode_times = LatencyWindow::new(2000);
    let mut gpu_times = LatencyWindow::new(2000);
    let mut pipeline_times = LatencyWindow::new(2000);
    let mut encoded_frames = 0u32;
    let mut decoded_frames = 0u32;
    let mut pending = 0u32;
    let mut decode_errors = 0u32;
    // Разброс яркости первого прочитанного кадра — доказательство,
    // что в текстуре настоящая картинка, а не пустота.
    let mut luma_spread: Option<(u8, u8)> = None;

    while now().duration_since(started) < deadline {
        let frame = match capturer.next_frame(frame_timeout) {
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

        // Кадр, где изменился только курсор, кодировать незачем (§5.1).
        if !frame.info().content_changed {
            continue;
        }

        let input = EncoderInput {
            texture: frame.texture().as_raw(),
            info: *frame.info(),
        };

        let Some(encoded) = encoder.encode(&input)? else {
            continue;
        };
        encoded_frames += 1;

        let before_decode = now();
        match decoder.decode(&encoded) {
            Ok(Some(decoded)) => {
                decode_times.push(now().duration_since(before_decode));

                // Ждём, пока GPU закончит писать в текстуру. Без этого
                // цифра выше — стоимость вызова, а не декода.
                // SAFETY: контекст и query живы; End завершает
                // измерение, начатое неявно созданием EVENT-query.
                unsafe { context.End(&query) };

                // GetData возвращает S_FALSE, пока GPU не закончил.
                // Проверять через Result нельзя: windows-rs считает
                // успехом любой неотрицательный HRESULT, а S_FALSE
                // как раз такой — цикл завершился бы сразу.
                let mut done = 0u32;
                loop {
                    // SAFETY: query жив; `done` — живая переменная
                    // нужного размера (BOOL для EVENT-query).
                    let hr = unsafe {
                        context.GetData(
                            &query,
                            Some(std::ptr::addr_of_mut!(done).cast()),
                            std::mem::size_of::<u32>() as u32,
                            0,
                        )
                    };
                    match hr {
                        Ok(()) if done != 0 => break,
                        Ok(()) => continue,
                        Err(e) => {
                            println!("GetData: {e}");
                            break;
                        }
                    }
                }
                gpu_times.push(now().duration_since(before_decode));

                decoded_frames += 1;

                if let Some(span) = decoded.info().timings.span(Stage::Captured, Stage::Decoded) {
                    pipeline_times.push(span);
                }

                // Один раз за прогон читаем пиксели. Без этого проба
                // доказывает лишь, что декодер отвечает без ошибок —
                // а он мог бы отдавать пустые поверхности, и цифры
                // выглядели бы столь же прилично.
                if luma_spread.is_none() {
                    luma_spread = inspect_luma(&device, &context, &decoded);
                }
            }
            Ok(None) => {
                // До первого ключевого кадра — норма.
                pending += 1;
            }
            Err(err) if err.needs_keyframe() => {
                // Потеря синхронизации лечится ключевым кадром, а не
                // пересозданием декодера (CLAUDE.md §5.2).
                decode_errors += 1;
                encoder.request_keyframe();
            }
            Err(err) => {
                println!("Ошибка декодирования: {err}");
                break;
            }
        }
    }

    let elapsed = now().duration_since(started);

    println!("=== Результат ===");
    if decoded_frames == 0 {
        println!("Кадров не декодировано.");
        println!("Закодировано: {encoded_frames}, придержано декодером: {pending}");
        if encoded_frames == 0 {
            println!("Экран не менялся — повторите, создав движение.");
        }
        return Ok(());
    }

    let secs = elapsed.as_secs_f64();
    println!("Время замера:      {secs:.1} с");
    println!("Закодировано:      {encoded_frames}");
    println!(
        "Декодировано:      {decoded_frames} ({:.1} fps)",
        decoded_frames as f64 / secs
    );
    println!("Придержано:        {pending} (ждали ключевого кадра)");
    println!("Ошибок потока:     {decode_errors}");

    match luma_spread {
        Some((min, max)) if max > min => {
            println!("Содержимое кадра:  яркость {min}..{max} — картинка есть");
        }
        Some((min, max)) => {
            println!("Содержимое кадра:  яркость {min}..{max} — ОДНОТОННО!");
            println!("  Декодер отдаёт пустую поверхность. Цифры ниже");
            println!("  ничего не значат, пока это не исправлено.");
        }
        None => {
            println!("Содержимое кадра:  прочитать не удалось");
        }
    }

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;

    if let (Some(med), Some(p95), Some(max)) = (
        decode_times.median(),
        decode_times.p95(),
        decode_times.max(),
    ) {
        println!();
        println!("Вызов декодера (без ожидания GPU):");
        println!("  медиана: {:.2} мс", ms(med));
        println!("  p95:     {:.2} мс", ms(p95));
        println!("  максимум:{:.2} мс", ms(max));
        println!("  Это стоимость вызова, а не декода: GPU работает");
        println!("  асинхронно. Настоящая цифра — ниже.");
    }

    if let (Some(med), Some(p95), Some(max)) =
        (gpu_times.median(), gpu_times.p95(), gpu_times.max())
    {
        println!();
        println!("Декод до готовности кадра в GPU (бюджет 3–5 мс):");
        println!("  медиана: {:.2} мс", ms(med));
        println!("  p95:     {:.2} мс", ms(p95));
        println!("  максимум:{:.2} мс", ms(max));

        let verdict = if ms(med) <= 5.0 {
            "В БЮДЖЕТЕ"
        } else {
            "ВЫШЕ БЮДЖЕТА — разбираться до следующего шага"
        };
        println!("  вердикт: {verdict}");
    }

    if let (Some(med), Some(p95)) = (pipeline_times.median(), pipeline_times.p95()) {
        println!();
        println!("Захват → энкод → декод суммарно:");
        println!("  медиана: {:.2} мс", ms(med));
        println!("  p95:     {:.2} мс", ms(p95));
        println!();
        println!("Осталось за кадром: сеть и рендер. Критерий этапа 1 —");
        println!("≤ 25 мс glass-to-glass (docs/latency-budget.md).");
    }

    Ok(())
}

/// Прочитать Y-плоскость декодированного кадра и вернуть минимум
/// и максимум яркости.
///
/// Единственная в пробе копия GPU→CPU, и она намеренная: без чтения
/// пикселей проба доказывала бы только то, что декодер не возвращает
/// ошибок. Пустая поверхность выглядела бы точно так же. В продукте
/// такой копии быть не должно (CLAUDE.md §4.2.3) — здесь это отладочный
/// путь, выполняемый один раз за прогон.
///
/// Возвращает `None`, если прочитать не удалось: диагностика не повод
/// ронять замер.
#[cfg(all(windows, nvenc_available))]
fn inspect_luma(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    frame: &bd_codec::mediafoundation::DecodedFrame,
) -> Option<(u8, u8)> {
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: текстура жива; GetDesc заполняет переданную структуру.
    unsafe { frame.texture().GetDesc(&mut desc) };

    // Staging-копия: читать напрямую из декодерной поверхности нельзя,
    // у неё нет доступа с CPU.
    let staging_desc = D3D11_TEXTURE2D_DESC {
        ArraySize: 1,
        MipLevels: 1,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };

    let mut staging: Option<ID3D11Texture2D> = None;
    // SAFETY: устройство живо; описание и выходной параметр — живые
    // локальные переменные.
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.ok()?;
    let staging = staging?;

    // SAFETY: обе текстуры живы и одного формата; исходный подресурс
    // сообщён самим декодером.
    unsafe {
        context.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            frame.texture(),
            frame.subresource(),
            None,
        )
    };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: staging создана с CPU_ACCESS_READ и USAGE_STAGING —
    // только такие ресурсы допускают Map для чтения.
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.ok()?;

    let mut min = u8::MAX;
    let mut max = u8::MIN;

    // Каждая 16-я строка и каждый 16-й пиксель: для «есть ли картинка»
    // этого более чем достаточно, а полный проход по 2 МБ ради
    // диагностики не нужен.
    for y in (0..desc.Height).step_by(16) {
        // SAFETY: `pData` действителен до Unmap; смещение не выходит
        // за пределы отображения — строка y существует, а шаг строки
        // сообщён самим Map.
        let row = unsafe { (mapped.pData as *const u8).add(y as usize * mapped.RowPitch as usize) };
        for x in (0..desc.Width).step_by(16) {
            // SAFETY: x меньше ширины, а значит и шага строки.
            let value = unsafe { *row.add(x as usize) };
            min = min.min(value);
            max = max.max(value);
        }
    }

    // SAFETY: парный Unmap к успешному Map выше.
    unsafe { context.Unmap(&staging, 0) };

    Some((min, max))
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
