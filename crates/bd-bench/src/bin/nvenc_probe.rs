//! Проба NVENC: загружается ли API и что умеет GPU.
//!
//! Отвечает на вопрос этапа 1: доступен ли аппаратный энкодер и какие
//! кодеки он поддерживает. Продукт требует аппаратного энкода
//! (CLAUDE.md §1), поэтому отрицательный ответ здесь — блокер для роли
//! хоста, а не мелкая неприятность.
//!
//! Запуск: `cargo run --release -p bd-bench --bin nvenc_probe`

// Проба открывает D3D11-устройство напрямую, поэтому unsafe здесь
// неизбежен. Это инструмент замера, а не часть продукта.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(windows, nvenc_available)))]
fn main() {
    println!("Проба NVENC требует Windows и собранного бэкенда NVENC.");
    println!();
    println!("Если SDK установлен, но проба не собралась — проверьте");
    println!("переменную NVENC_SDK_PATH и пересоберите:");
    println!("  cargo build --release -p bd-bench");
}

#[cfg(all(windows, nvenc_available))]
fn main() -> anyhow::Result<()> {
    use bd_codec::nvenc::NvencApi;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== Проба NVENC ===\n");

    match NvencApi::get() {
        Ok(_api) => {
            println!("NVENC API загружен успешно.");
            println!();
            println!("Примечание: сверяется только мажорная версия API.");
            println!("NVENC даёт бинарную обратную совместимость, поэтому");
            println!("расхождение в минорной версии — не препятствие.");
        }
        Err(err) => {
            println!("NVENC НЕДОСТУПЕН: {err}");
            println!();
            if err.should_try_next_backend() {
                println!("Имеет смысл пробовать AMF (AMD) или QuickSync (Intel).");
                println!("Порядок бэкендов — CLAUDE.md §5.2.");
            }
            return Ok(());
        }
    }

    // Загрузки API мало: она не проверяет ни версии структур, ни GUID.
    // Настоящая проверка — открыть сессию на реальном устройстве.
    println!();
    println!("Открываем сессию кодирования на D3D11-устройстве...");
    match open_test_session() {
        Ok(()) => {
            println!();
            println!("СЕССИЯ ОТКРЫТА И НАСТРОЕНА.");
            println!("Версии структур и GUID верны, аппаратный энкод доступен.");
        }
        Err(err) => {
            println!();
            println!("Не удалось открыть сессию: {err}");
            println!();
            println!("Частая причина — неверная версия структуры или GUID");
            println!("(crates/bd-codec/src/nvenc/versions.rs и guids.rs).");
            println!("Такие ошибки не ловятся компилятором, только рантаймом.");
        }
    }

    Ok(())
}

/// Открыть и сразу закрыть сессию NVENC на временном D3D11-устройстве.
#[cfg(all(windows, nvenc_available))]
fn open_test_session() -> anyhow::Result<()> {
    use bd_codec::nvenc::NvencSession;
    use bd_codec::EncoderConfig;
    use bd_core::frame::FrameSize;
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };

    let mut device: Option<ID3D11Device> = None;
    let levels = [D3D_FEATURE_LEVEL_11_0];

    // SAFETY: выходной параметр — валидная локальная переменная;
    // адаптер не указан, поэтому драйвер выбирается по типу HARDWARE.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }

    let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 не создал устройство"))?;
    let config = EncoderConfig::low_latency(FrameSize::new(1920, 1080), 60);

    println!(
        "  кодек: H.264, 1920x1080@60, битрейт {} Мбит/с",
        config.rate_control.target_bitrate() / 1_000_000
    );

    // SAFETY: `device` живёт до конца функции, то есть переживает сессию,
    // которая уничтожается в конце области видимости.
    let session = unsafe { NvencSession::open_d3d11(device.as_raw(), config) }?;
    drop(session);

    Ok(())
}
