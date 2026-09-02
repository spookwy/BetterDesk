//! Проба уменьшения кадра на GPU.
//!
//! # Что она проверяет и почему этого нельзя проверить тестом
//!
//! Шейдер компилируется драйвером на живой машине. Юнит-тест здесь
//! бесполезен: он проверил бы, что код собирается, — а собирается он
//! и с неверным шейдером, и с перепутанными слотами, и с текстурой
//! без нужных флагов. Всё это даёт не ошибку компиляции, а отказ на
//! чужом железе либо чёрный кадр (находки 21, 23, 50).
//!
//! Поэтому проба делает три вещи, которых тест сделать не может:
//!
//! 1. компилирует шейдер настоящим драйвером;
//! 2. уменьшает настоящий кадр экрана;
//! 3. **читает пиксели результата** и проверяет, что там картинка, а
//!    не пустота.
//!
//! Третий пункт — главный. Уменьшитель, отдающий чёрные текстуры,
//! выглядит исправным: ошибок нет, тайминги отличные. Ровно тот же
//! урок, что в находке 21 у декодера.
//!
//! Запуск: `cargo run --release -p bd-bench --bin downscale_probe`

#![cfg(windows)]

use bd_capture::windows::{enumerate_monitors, Downscaler, DxgiCapturer};
use bd_capture::{CaptureOutcome, Capturer};
use bd_core::time::Epoch;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

fn main() -> anyhow::Result<()> {
    println!("=== Проба уменьшения кадра ===\n");

    let monitors = enumerate_monitors()?;
    let Some(monitor) = monitors.first() else {
        anyhow::bail!("мониторов не найдено");
    };
    println!(
        "Монитор: {} — {}x{}",
        monitor.device_name, monitor.size.width, monitor.size.height
    );

    let epoch = Epoch::new();
    let mut capturer = DxgiCapturer::new(monitor.index, epoch)?;
    let full = capturer.size();
    println!("Захват: {}x{}\n", full.width, full.height);

    let mut downscaler = Downscaler::new(capturer.device())?;
    println!("✅ Шейдер уменьшения скомпилирован драйвером\n");

    // Кадр приходит только на изменение экрана (находка 8), поэтому
    // ждём с запасом и объясняем, если не дождались.
    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if Instant::now() > deadline {
            println!("❌ За пять секунд экран не изменился.");
            println!("   Пошевелите окном и запустите снова: DXGI отдаёт");
            println!("   кадр только на изменение картинки, это норма.");
            std::process::exit(1);
        }
        match capturer.next_frame(Duration::from_millis(100)) {
            Ok(CaptureOutcome::Frame(f)) => break f,
            Ok(_) => continue,
            Err(e) if e.is_recoverable() => continue,
            Err(e) => anyhow::bail!("захват сломался: {e}"),
        }
    };

    let mut failures = 0;

    for divisor in [2u32, 4] {
        let started = Instant::now();
        let small = downscaler.downscale(
            capturer.device(),
            capturer.context(),
            frame.texture(),
            divisor,
        )?;
        let elapsed = started.elapsed();

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: текстура жива; GetDesc заполняет переданную структуру.
        unsafe { small.GetDesc(&mut desc) };

        let expected_w = (full.width / divisor).max(2) & !1;
        let expected_h = (full.height / divisor).max(2) & !1;

        if desc.Width != expected_w || desc.Height != expected_h {
            println!(
                "❌ /{divisor}: размер {}x{}, ожидался {expected_w}x{expected_h}",
                desc.Width, desc.Height
            );
            failures += 1;
            continue;
        }

        // Главная проверка: в кадре есть картинка, а не пустота.
        let spread = luminance_spread(capturer.device(), capturer.context(), small, &desc)?;
        if spread < 8 {
            println!(
                "❌ /{divisor}: {}x{} за {:.2} мс, но разброс яркости {spread} —\n     \
                 похоже, текстура пустая (см. находку 21)",
                desc.Width,
                desc.Height,
                elapsed.as_secs_f64() * 1000.0
            );
            failures += 1;
            continue;
        }

        println!(
            "✅ /{divisor}: {}x{} за {:.2} мс, разброс яркости {spread}",
            desc.Width,
            desc.Height,
            elapsed.as_secs_f64() * 1000.0
        );
    }

    println!();
    if failures > 0 {
        println!("❌ Уменьшение неисправно.");
        std::process::exit(1);
    }
    println!("✅ Уменьшение работает: размеры верны, картинка на месте.");
    Ok(())
}

/// Разброс яркости в кадре: максимум минус минимум.
///
/// Ноль означает однотонную заливку — то есть, скорее всего, пустую
/// текстуру. Настоящий экран всегда даёт разброс в десятки единиц.
fn luminance_spread(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    desc: &D3D11_TEXTURE2D_DESC,
) -> anyhow::Result<u32> {
    // Читать пиксели можно только через промежуточную staging-копию:
    // текстура цели живёт в видеопамяти и CPU её не видит.
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..*desc
    };

    let mut staging: Option<ID3D11Texture2D> = None;
    // SAFETY: описание заполнено; начальных данных нет.
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.ok_or_else(|| anyhow::anyhow!("staging-текстура не создана"))?;

    // SAFETY: обе текстуры живы и совпадают по формату и размеру.
    unsafe { context.CopyResource(&staging, texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: staging создана с CPU_ACCESS_READ; отображается
    // нулевой подресурс, он единственный.
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

    let mut min = u32::MAX;
    let mut max = 0u32;
    // SAFETY: `pData` действителен между Map и Unmap; шагаем строго в
    // пределах заявленных ширины, высоты и RowPitch.
    unsafe {
        let base = mapped.pData as *const u8;
        for y in 0..desc.Height {
            let row = base.add((y * mapped.RowPitch) as usize);
            for x in 0..desc.Width {
                let px = row.add((x * 4) as usize);
                // BGRA: грубая яркость без точных коэффициентов —
                // здесь важен сам факт разброса, а не колориметрия.
                let luma = *px as u32 + *px.add(1) as u32 + *px.add(2) as u32;
                min = min.min(luma);
                max = max.max(luma);
            }
        }
        context.Unmap(&staging, 0);
    }

    Ok(max.saturating_sub(min))
}
