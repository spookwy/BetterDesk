//! Swapchain и его настройка под минимальную задержку.
//!
//! # Три решения, каждое из которых стоит кадров
//!
//! - **`SetMaximumFrameLatency(1)`.** По умолчанию DXGI разрешает
//!   очередь из трёх кадров — это 33–50 мс задержки на ровном месте
//!   (docs/latency-budget.md §3). Единица означает: следующий кадр
//!   не принимается, пока предыдущий не показан.
//! - **`Present(0, ...)` без VSync.** Ожидание вертикальной синхронизации
//!   добавляет до 16.6 мс. Тиринг при удалённой работе предпочтительнее
//!   лага (CLAUDE.md §5.5).
//! - **`FLIP_DISCARD`.** Модель без копирования содержимого буфера:
//!   DWM берёт наш буфер напрямую, вместо того чтобы копировать его
//!   в свой.
//!
//! Waitable object намеренно **не** используется: он даёт ровный
//! ритм, синхронизируя нас с дисплеем, но кадры приходят от декодера,
//! а не от дисплея. Ждать на нём значило бы добавить к задержке ровно
//! то, что мы экономим на VSync.

use crate::{RenderError, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11RenderTargetView, ID3D11Texture2D, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice1, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

/// Число буферов.
///
/// Два: один показывается, во второй рисуем. Три дали бы более ровный
/// ритм ценой лишнего кадра задержки — не наш размен.
const BUFFER_COUNT: u32 = 2;

/// Цепочка буферов вывода.
pub struct SwapChain {
    swapchain: IDXGISwapChain1,
    /// Представление текущего заднего буфера.
    ///
    /// Пересоздаётся при каждом изменении размера: старое ссылается
    /// на освобождённый буфер, и `ResizeBuffers` не выполнится, пока
    /// такая ссылка жива.
    rtv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
}

impl SwapChain {
    /// Создать swapchain для окна.
    pub fn new(device: &ID3D11Device, hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let dxgi_device: IDXGIDevice1 = device
            .cast()
            .map_err(|e| RenderError::SwapChain(format!("IDXGIDevice1 недоступен: {e}")))?;

        // Главная строка этого модуля. По умолчанию было бы 3.
        // SAFETY: интерфейс получен из живого устройства.
        unsafe { dxgi_device.SetMaximumFrameLatency(1) }
            .map_err(|e| RenderError::SwapChain(format!("SetMaximumFrameLatency: {e}")))?;

        // SAFETY: устройство живо; GetAdapter не принимает входных
        // указателей.
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|e| RenderError::SwapChain(format!("GetAdapter: {e}")))?;

        // SAFETY: адаптер жив; тип фабрики задан параметром типа.
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
            .map_err(|e| RenderError::SwapChain(format!("GetParent(IDXGIFactory2): {e}")))?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFER_COUNT,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };

        // SAFETY: устройство и окно живы; описание живёт до конца
        // вызова; полноэкранное описание не нужно — оконный режим.
        let swapchain = unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }
            .map_err(|e| RenderError::SwapChain(format!("CreateSwapChainForHwnd: {e}")))?;

        tracing::info!(width, height, "swapchain создан (FLIP_DISCARD, latency 1)");

        let mut this = Self {
            swapchain,
            rtv: None,
            width: desc.Width,
            height: desc.Height,
        };
        this.create_rtv(device)?;
        Ok(this)
    }

    /// Представление заднего буфера для отрисовки.
    pub fn render_target(&self) -> Option<&ID3D11RenderTargetView> {
        self.rtv.as_ref()
    }

    /// Задний буфер как DXGI-поверхность — для Direct2D.
    ///
    /// Оверлей рисует текст поверх уже выведенного кадра в ту же
    /// поверхность, без промежуточных текстур (CLAUDE.md §6.1).
    pub fn back_buffer_surface(&self) -> Result<IDXGISurface> {
        // SAFETY: swapchain жив; нулевой буфер существует всегда,
        // тип запрошен параметром и поддерживается задним буфером.
        unsafe { self.swapchain.GetBuffer(0) }.map_err(|e| RenderError::Platform {
            context: "GetBuffer(IDXGISurface)",
            hresult: e.code().0 as u32,
        })
    }

    /// Область вывода во весь буфер.
    pub fn viewport(&self) -> D3D11_VIEWPORT {
        D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }
    }

    /// Подогнать под новый размер окна, если он изменился.
    ///
    /// Ничего не делает, когда размер прежний: `ResizeBuffers` —
    /// операция не бесплатная, и звать её каждый кадр нельзя.
    pub fn resize(&mut self, device: &ID3D11Device, width: u32, height: u32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.width && height == self.height {
            return Ok(());
        }

        // Представление держит ссылку на буфер: пока оно живо,
        // ResizeBuffers вернёт ошибку.
        self.rtv = None;

        // SAFETY: swapchain жив; ссылок на его буферы не осталось —
        // единственную мы только что отпустили.
        unsafe {
            self.swapchain.ResizeBuffers(
                BUFFER_COUNT,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|e| map_device_error("ResizeBuffers", e))?;

        self.width = width;
        self.height = height;
        self.create_rtv(device)?;

        tracing::debug!(width, height, "swapchain пересоздан под новый размер");
        Ok(())
    }

    /// Показать кадр.
    ///
    /// Без VSync: `syncinterval = 0`. Ждать вертикальной синхронизации
    /// значит добавить до 16.6 мс к задержке.
    pub fn present(&self) -> Result<()> {
        // SAFETY: swapchain жив; Present возвращает HRESULT напрямую,
        // а не Result — ошибку разбираем сами.
        let hr = unsafe { self.swapchain.Present(0, DXGI_PRESENT(0)) };

        if hr.is_ok() {
            return Ok(());
        }

        if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
            return Err(RenderError::DeviceLost(format!(
                "Present вернул {:#010x}",
                hr.0 as u32
            )));
        }

        Err(RenderError::Platform {
            context: "Present",
            hresult: hr.0 as u32,
        })
    }

    /// Создать представление для текущего заднего буфера.
    fn create_rtv(&mut self, device: &ID3D11Device) -> Result<()> {
        // SAFETY: swapchain жив; нулевой буфер существует всегда,
        // тип запрошен параметром и соответствует его содержимому.
        let back_buffer: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0) }
            .map_err(|e| RenderError::SwapChain(format!("GetBuffer: {e}")))?;

        let mut rtv = None;
        // SAFETY: устройство и буфер живы; описание не нужно —
        // представление наследует формат ресурса.
        unsafe { device.CreateRenderTargetView(&back_buffer, None, Some(&mut rtv)) }
            .map_err(|e| RenderError::SwapChain(format!("CreateRenderTargetView: {e}")))?;

        self.rtv = rtv;
        Ok(())
    }
}

/// Отличить потерю устройства от обычной ошибки.
///
/// Потеря устройства требует пересоздания всего стека, а не повтора
/// вызова, поэтому эти случаи нельзя смешивать.
fn map_device_error(context: &'static str, error: windows::core::Error) -> RenderError {
    let code = error.code();
    if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        return RenderError::DeviceLost(format!("{context}: {error}"));
    }
    RenderError::Platform {
        context,
        hresult: code.0 as u32,
    }
}
