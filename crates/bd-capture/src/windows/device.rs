//! Создание D3D11-устройства и перечисление мониторов.
//!
//! Устройство создаётся на том же адаптере, к которому подключён
//! захватываемый монитор. Это требование DXGI Desktop Duplication:
//! дупликация работает только когда процесс исполняется на адаптере
//! дисплея. На гибридной графике это и есть источник отказов
//! (CLAUDE.md §5.1) — тогда система переходит на WGC.

use crate::{CaptureError, MonitorInfo, Result};
use bd_core::frame::FrameSize;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
};

/// D3D11-устройство и его контекст.
///
/// RAII-обёртка (CLAUDE.md §4.3.4): COM-объекты внутри освобождаются
/// автоматически при уничтожении — `windows`-крейт реализует `Drop`
/// для интерфейсов через подсчёт ссылок COM.
pub struct D3dDevice {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Адаптер, на котором создано устройство. Нужен для повторного
    /// создания дупликации после потери доступа.
    adapter: IDXGIAdapter1,
}

impl D3dDevice {
    /// Создать устройство на адаптере, к которому подключён монитор
    /// с индексом `monitor_index`.
    ///
    /// Возвращает устройство и выход (`IDXGIOutput1`) для дупликации.
    pub fn for_monitor(monitor_index: u32) -> Result<(Self, IDXGIOutput1)> {
        let (adapter, output) = find_adapter_for_monitor(monitor_index)?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();

        // Уровни от старшего к младшему: берётся первый поддерживаемый.
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        // SAFETY: `adapter` — живой COM-интерфейс, полученный из
        // EnumAdapters1 и удерживаемый до конца вызова. При явно
        // указанном адаптере драйвер обязан быть D3D_DRIVER_TYPE_UNKNOWN
        // (требование D3D11CreateDevice). Выходные указатели — валидные
        // локальные Option, которые заполняет функция.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                // BGRA_SUPPORT нужен, потому что дупликация отдаёт BGRA.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
        }
        .map_err(|e| CaptureError::DeviceInit(format!("D3D11CreateDevice: {e}")))?;

        let device = device.ok_or_else(|| {
            CaptureError::DeviceInit("D3D11CreateDevice вернул пустое устройство".into())
        })?;
        let context = context.ok_or_else(|| {
            CaptureError::DeviceInit("D3D11CreateDevice вернул пустой контекст".into())
        })?;

        tracing::debug!(?feature_level, monitor_index, "D3D11-устройство создано");

        Ok((
            Self {
                device,
                context,
                adapter,
            },
            output,
        ))
    }

    /// Создать устройство на адаптере по умолчанию, без привязки
    /// к монитору и без дупликации.
    ///
    /// # Зачем отдельно от `for_monitor`
    ///
    /// Клиенту захват не нужен: он принимает чужой экран, декодирует
    /// и показывает (CLAUDE.md §3.1). Но декодер и окно вывода живут
    /// на D3D11-устройстве, а единственный способ его получить до сих
    /// пор шёл через `DxgiCapturer`, то есть через дупликацию
    /// **своего** монитора.
    ///
    /// Следствие практическое, а не эстетическое: клиент не мог
    /// запуститься там, где дупликация недоступна, — например на
    /// гибридной графике (§5.1), — хотя дуплицировать ему нечего.
    /// Роль объявлена в §3.1, но порядок инициализации ей
    /// противоречил: тот же класс ошибки, что находка 47, где клиент
    /// требовал энкодера, чтобы ничего не кодировать.
    ///
    /// Адаптер берётся первый: у клиента нет монитора, чей адаптер
    /// надо было бы угадывать, а декодер и swapchain работают на любом.
    pub fn for_decode() -> Result<Self> {
        // SAFETY: CreateDXGIFactory1 не принимает входных указателей;
        // тип фабрики задан параметром типа и корректен.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| CaptureError::DeviceInit(format!("CreateDXGIFactory1: {e}")))?;

        // SAFETY: `factory` жив. Адаптер 0 есть на любой машине с
        // работающим DXGI; его отсутствие означает отсутствие GPU.
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(0) }
            .map_err(|e| CaptureError::DeviceInit(format!("адаптеров не найдено: {e}")))?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();

        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        // SAFETY: тот же контракт, что в `for_monitor` — адаптер жив и
        // указан явно, поэтому тип драйвера обязан быть UNKNOWN.
        // BGRA_SUPPORT нужен Direct2D: на нём рисуются оверлей и курсор.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
        }
        .map_err(|e| CaptureError::DeviceInit(format!("D3D11CreateDevice: {e}")))?;

        let device = device.ok_or_else(|| {
            CaptureError::DeviceInit("D3D11CreateDevice вернул пустое устройство".into())
        })?;
        let context = context.ok_or_else(|| {
            CaptureError::DeviceInit("D3D11CreateDevice вернул пустой контекст".into())
        })?;

        tracing::debug!(?feature_level, "D3D11-устройство для декода создано");

        Ok(Self {
            device,
            context,
            adapter,
        })
    }

    /// Ссылка на устройство.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Ссылка на непосредственный контекст.
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Адаптер, на котором создано устройство.
    pub fn adapter(&self) -> &IDXGIAdapter1 {
        &self.adapter
    }
}

// SAFETY: ID3D11Device потокобезопасен по умолчанию (D3D11 поддерживает
// многопоточный доступ к устройству, если не запрошен флаг SINGLETHREADED,
// а он не запрошен). ID3D11DeviceContext потокобезопасным НЕ является,
// поэтому `D3dDevice` можно перемещать между потоками (Send), но не
// использовать из нескольких одновременно (не Sync).
unsafe impl Send for D3dDevice {}

/// Найти только выход для монитора, не создавая устройство.
///
/// Нужен восстановлению после `ACCESS_LOST`: там дупликация
/// пересоздаётся на **существующем** устройстве. Пересоздать
/// устройство значило бы оставить энкодер, декодер и окно вывода
/// с мёртвым указателем — они все живут на устройстве захвата.
pub(crate) fn output_for_monitor(monitor_index: u32) -> Result<IDXGIOutput1> {
    find_adapter_for_monitor(monitor_index).map(|(_, output)| output)
}

/// Найти адаптер и выход для монитора с заданным индексом.
///
/// Индексация сквозная по всем адаптерам: сначала выходы первого
/// адаптера, затем второго и так далее.
fn find_adapter_for_monitor(monitor_index: u32) -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    // SAFETY: CreateDXGIFactory1 не принимает входных указателей;
    // тип фабрики задан параметром типа и корректен.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| CaptureError::DeviceInit(format!("CreateDXGIFactory1: {e}")))?;

    let mut seen = 0u32;

    for adapter_index in 0.. {
        // SAFETY: `factory` жив; EnumAdapters1 возвращает
        // DXGI_ERROR_NOT_FOUND по исчерпании адаптеров — это условие выхода.
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(_) => break,
        };

        for output_index in 0.. {
            // SAFETY: `adapter` жив; EnumOutputs сигнализирует об
            // окончании перечисления ошибкой DXGI_ERROR_NOT_FOUND.
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };

            if seen == monitor_index {
                let output1: IDXGIOutput1 = output.cast().map_err(|e| {
                    CaptureError::Unsupported(format!("IDXGIOutput1 недоступен: {e}"))
                })?;
                return Ok((adapter, output1));
            }
            seen += 1;
        }
    }

    Err(CaptureError::MonitorNotFound {
        index: monitor_index,
        available: seen,
    })
}

/// Перечислить все мониторы всех адаптеров.
pub(crate) fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    // SAFETY: см. find_adapter_for_monitor — тот же контракт.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| CaptureError::DeviceInit(format!("CreateDXGIFactory1: {e}")))?;

    let mut monitors = Vec::new();
    let mut index = 0u32;

    for adapter_index in 0.. {
        // SAFETY: как выше — ошибка означает конец перечисления.
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(_) => break,
        };

        for output_index in 0.. {
            // SAFETY: как выше.
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };

            // SAFETY: `output` жив; GetDesc возвращает описание по значению
            // и не принимает входных указателей.
            let desc = match unsafe { output.GetDesc() } {
                Ok(d) => d,
                Err(_) => continue,
            };

            let rect = desc.DesktopCoordinates;
            let size = FrameSize::new(
                (rect.right - rect.left).max(0) as u32,
                (rect.bottom - rect.top).max(0) as u32,
            );

            // Имя устройства — массив UTF-16, дополненный нулями.
            let name_len = desc
                .DeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.DeviceName.len());
            let device_name = String::from_utf16_lossy(&desc.DeviceName[..name_len]);

            monitors.push(MonitorInfo {
                index,
                device_name,
                size,
                origin: (rect.left, rect.top),
                // Основной монитор — тот, чей левый верхний угол в (0,0).
                is_primary: rect.left == 0 && rect.top == 0,
            });
            index += 1;
        }
    }

    Ok(monitors)
}
