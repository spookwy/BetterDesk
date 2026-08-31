//! Вывод кадров на Windows: окно, swapchain, конверсия NV12→RGB.
//!
//! # Устройство модуля
//!
//! - `window` — нативное окно Win32 и неблокирующий насос сообщений;
//! - `swapchain` — цепочка буферов, настроенная под минимальную
//!   задержку;
//! - `pipeline` — шейдер NV12→RGB и вывод кадра.
//!
//! Правило слоёв (CLAUDE.md §4.3.3): сырые указатели наружу не
//! выходят. Верхние уровни видят [`VideoWindow`] и передают ему
//! текстуру декодера.

mod cursor;
mod overlay;
mod pipeline;
mod swapchain;
mod window;

use crate::{RenderError, Result};
use bd_core::frame::FrameSize;
use bd_core::metrics::Stage;
use bd_core::time::Epoch;
use overlay::StatsOverlay;
use pipeline::NvToRgbPipeline;
use swapchain::SwapChain;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};

pub use cursor::{to_bgra, CursorRenderer};
pub use window::{RawInputMessage, SessionWindow};

/// Окно с видеопотоком: всё, что нужно для показа кадров.
///
/// # Порядок полей важен
///
/// Rust уничтожает поля сверху вниз. Swapchain держит окно, а
/// конвейер и контекст — устройство. Окно объявлено последним, чтобы
/// пережить всё, что на него ссылается. Ошибка здесь тихая: она даёт
/// не падение, а сообщения драйвера при выходе (тот же класс, что
/// находка 16 в CLAUDE.md §0.1).
pub struct VideoWindow {
    overlay: StatsOverlay,
    /// Курсор рисуется поверх кадра, а не вморожен в него: картинка
    /// отстаёт на задержку пайплайна, а позиция курсора приходит
    /// отдельным пакетом и успевает раньше (docs/roadmap.md, этап 2).
    cursor: CursorRenderer,
    /// Где курсор сейчас, в долях экрана, и виден ли он.
    cursor_position: Option<(f32, f32)>,
    /// Копия последнего кадра и его видимый размер.
    ///
    /// Нужна, чтобы перерисовать картинку, когда новых кадров нет:
    /// DXGI отдаёт их только при изменении экрана, а окно должно
    /// оставаться живым и показывать движущийся курсор
    /// (см. [`Self::redraw_last_frame`]).
    last_frame: Option<(ID3D11Texture2D, FrameSize)>,
    pipeline: NvToRgbPipeline,
    swapchain: SwapChain,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    window: SessionWindow,
    epoch: Epoch,
    /// Размер, с которым создан swapchain. Сравнивается с размером
    /// клиентской области каждый кадр — дёшево, а окно может менять
    /// размер в любой момент.
    presented_size: (u32, u32),
}

impl VideoWindow {
    /// Создать окно для потока заданного разрешения.
    ///
    /// Устройство должно быть тем же, на котором работает декодер:
    /// иначе текстуру пришлось бы копировать между устройствами и
    /// zero-copy потерялся бы (CLAUDE.md §4.2.3).
    pub fn new(device: &ID3D11Device, title: &str, size: FrameSize, epoch: Epoch) -> Result<Self> {
        if size.is_empty() {
            return Err(RenderError::WindowCreation(
                "разрешение потока нулевое".into(),
            ));
        }

        let window = SessionWindow::new(title, size.width, size.height)?;
        let (width, height) = window.client_size();

        let swapchain = SwapChain::new(device, window.hwnd(), width, height)?;
        let pipeline = NvToRgbPipeline::new(device)?;

        // SAFETY: устройство живо. В windows 0.62 контекст возвращается
        // значением, а не через out-параметр (CLAUDE.md §0.1, находка 6).
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| RenderError::SwapChain(format!("GetImmediateContext: {e}")))?;

        let overlay = StatsOverlay::new()?;

        Ok(Self {
            overlay,
            cursor: CursorRenderer::new(),
            cursor_position: None,
            last_frame: None,
            pipeline,
            swapchain,
            context,
            device: device.clone(),
            window,
            epoch,
            presented_size: (width, height),
        })
    }

    /// Прокачать очередь сообщений. `false` — пользователь закрыл окно.
    ///
    /// Вызывать каждый кадр: иначе окно перестанет отвечать.
    pub fn pump_messages(&mut self) -> bool {
        let alive = self.window.pump_messages();

        // Горячая клавиша оверлея обрабатывается здесь, а не в оконной
        // процедуре: та вызывается системой и не имеет доступа к
        // состоянию рендера.
        if self.window.take_overlay_toggle() {
            self.overlay.toggle();
        }

        alive
    }

    /// Виден ли оверлей статистики.
    pub fn overlay_visible(&self) -> bool {
        self.overlay.is_visible()
    }

    /// Забрать накопленные сообщения ввода.
    ///
    /// Сообщения **сырые**: `bd-render` не знает формата событий
    /// ввода, потому что зависеть от `bd-input` ему нельзя
    /// (CLAUDE.md §4.2.5). Переводит их тот, кто видит оба крейта.
    pub fn drain_input(&self, out: &mut Vec<RawInputMessage>) {
        self.window.drain_input(out);
    }

    /// Сколько сообщений ввода потеряно из-за переполнения очереди.
    pub fn input_dropped(&self) -> u64 {
        self.window.input_dropped()
    }

    /// Размер клиентской области окна в пикселях.
    ///
    /// Нужен для нормализации координат мыши: событие несёт долю
    /// экрана, а система даёт пиксели окна.
    pub fn client_size(&self) -> (u32, u32) {
        self.window.client_size()
    }

    /// Задать положение курсора.
    ///
    /// Координаты — доли экрана хоста. `visible == false` прячет
    /// курсор: так бывает при полноэкранном видео или вводе текста,
    /// и рисовать стрелку там, где её нет, нельзя.
    ///
    /// Вызывать при каждом обновлении позиции — оно приходит отдельным
    /// пакетом, чаще, чем кадры.
    pub fn set_cursor_position(&mut self, position: &bd_core::cursor::CursorPosition) {
        self.cursor_position = position
            .visible
            .then(|| (position.position.x(), position.position.y()));
    }

    /// Загрузить новую форму курсора.
    ///
    /// Ошибку возвращает, но сессию она прерывать не должна: без
    /// курсора работать можно, без картинки — нет. Форма приходит
    /// по сети и может быть некорректной (CLAUDE.md §8.5).
    pub fn set_cursor_shape(&mut self, shape: &bd_core::cursor::CursorShape) -> Result<()> {
        // Цель вывода D2D создаётся при первой отрисовке, а битмап
        // привязан к ней. Значит, форму нельзя загрузить раньше, чем
        // появится поверхность, — сохраняем на следующий кадр.
        let surface = self.swapchain.back_buffer_surface()?;
        let target = self.overlay.render_target_for(&surface)?;
        self.cursor.set_shape(&target, shape)
    }

    /// Номер загруженной формы курсора.
    ///
    /// `None` означает, что форма ещё не пришла. Клиент в этом случае
    /// не рисует ничего и **не ждёт**: ожидание заморозило бы курсор.
    pub fn cursor_shape_id(&self) -> Option<u32> {
        self.cursor.shape_id()
    }

    /// Перерисовать последний показанный кадр.
    ///
    /// # Зачем это нужно
    ///
    /// DXGI отдаёт кадр только при изменении экрана (§5.1). На
    /// статичной картинке кадров нет вовсе — и без этого метода окно
    /// не перерисовывается совсем. Выглядит это как **зависшее окно**,
    /// которое «отвисает», стоит подвигать мышью: движение меняет
    /// экран, DXGI отдаёт кадр, картинка обновляется.
    ///
    /// `FLIP_DISCARD` не сохраняет содержимое буфера после `Present`,
    /// поэтому просто повторить `Present` нельзя — будет мусор. Кадр
    /// хранится в собственной текстуре и рисуется заново.
    ///
    /// Нужен ещё и потому, что **курсор движется чаще, чем картинка**:
    /// без перерисовки его новая позиция не появилась бы на экране до
    /// следующего изменения содержимого.
    ///
    /// `false` означает, что показывать пока нечего — ни одного кадра
    /// ещё не было.
    pub fn redraw_last_frame(&mut self) -> Result<bool> {
        let Some((texture, visible)) = self.last_frame.clone() else {
            return Ok(false);
        };

        let mut timings = bd_core::metrics::FrameTimings::default();
        // Копия кадра хранится как отдельная текстура, поэтому
        // подресурс всегда нулевой.
        self.draw_and_present(&texture, 0, visible, &mut timings, &[])?;
        Ok(true)
    }

    /// Сохранить кадр для повторной отрисовки.
    ///
    /// Копия делается на GPU (`CopyResource`), в системную память
    /// кадр не попадает (CLAUDE.md §4.2.3). Текстура декодера для
    /// хранения не годится: MFT переиспользует пул поверхностей, и
    /// удержанный кадр был бы перезаписан следующим.
    fn remember_frame(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        visible: FrameSize,
    ) -> Result<()> {
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        // SAFETY: текстура жива; GetDesc только заполняет структуру.
        unsafe { texture.GetDesc(&mut desc) };

        // Своя текстура пересоздаётся только при смене геометрии:
        // на каждом кадре это было бы заметной тратой.
        let needs_new = match &self.last_frame {
            Some((existing, _)) => {
                let mut have =
                    windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                // SAFETY: та же причина, что выше.
                unsafe { existing.GetDesc(&mut have) };
                have.Width != desc.Width || have.Height != desc.Height || have.Format != desc.Format
            }
            None => true,
        };

        if needs_new {
            let copy_desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
                BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0
                    as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut created = None;
            // SAFETY: описание живёт до конца вызова; начальных данных
            // нет — текстура заполняется копированием ниже.
            unsafe {
                self.device
                    .CreateTexture2D(&copy_desc, None, Some(&mut created))
            }
            .map_err(|e| RenderError::SwapChain(format!("CreateTexture2D для копии: {e}")))?;

            let created =
                created.ok_or_else(|| RenderError::SwapChain("копия кадра не создана".into()))?;
            self.last_frame = Some((created, visible));
        } else if let Some((_, stored)) = self.last_frame.as_mut() {
            *stored = visible;
        }

        let Some((copy, _)) = self.last_frame.as_ref() else {
            return Ok(());
        };

        // SAFETY: обе текстуры живы и созданы на этом устройстве;
        // геометрия совпадает (проверена выше), поэтому копирование
        // среза в нулевой подресурс корректно.
        unsafe {
            self.context
                .CopySubresourceRegion(copy, 0, 0, 0, 0, texture, subresource, None);
        }
        Ok(())
    }

    /// Показать декодированный кадр.
    ///
    /// `texture` — NV12-поверхность декодера, `subresource` — индекс
    /// среза в ней, `visible` — настоящее разрешение картинки без
    /// выравнивающего дополнения.
    ///
    /// Отметка [`Stage::Presented`] ставится непосредственно перед
    /// `Present`: она и есть конец измеряемого пути (CLAUDE.md §4.5).
    pub fn present_frame(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        visible: FrameSize,
        timings: &mut bd_core::metrics::FrameTimings,
        stats: &[String],
    ) -> Result<()> {
        // Кадр запоминается до отрисовки: если она провалится,
        // перерисовывать всё равно будет что.
        self.remember_frame(texture, subresource, visible)?;
        self.draw_and_present(texture, subresource, visible, timings, stats)
    }

    /// Нарисовать кадр и показать его.
    ///
    /// Общая часть для нового кадра и для перерисовки последнего.
    fn draw_and_present(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        visible: FrameSize,
        timings: &mut bd_core::metrics::FrameTimings,
        stats: &[String],
    ) -> Result<()> {
        // Размер окна мог измениться с прошлого кадра.
        let client = self.window.client_size();
        if client != self.presented_size && client.0 > 0 && client.1 > 0 {
            // Цель вывода D2D держит поверхность заднего буфера, а
            // `ResizeBuffers` не выполнится, пока такая ссылка жива.
            // Битмап курсора привязан к той же цели, поэтому
            // отпускается вместе с ней — иначе он пережил бы свою
            // цель и рисовал бы в никуда.
            self.overlay.release_target();
            self.cursor.release();
            self.swapchain.resize(&self.device, client.0, client.1)?;
            self.presented_size = client;
        }

        let Some(rtv) = self.swapchain.render_target() else {
            return Err(RenderError::SwapChain(
                "нет цели вывода: swapchain не готов".into(),
            ));
        };

        let viewport = self.swapchain.viewport();

        // SAFETY: представление и область вывода живут до конца вызова;
        // глубинный буфер не нужен — рисуем полноэкранный треугольник.
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.context.RSSetViewports(Some(&[viewport]));
        }

        self.pipeline.draw(
            &self.device,
            &self.context,
            texture,
            subresource,
            (visible.width, visible.height),
        )?;

        // Оверлей и курсор поверх уже нарисованного кадра, в ту же
        // поверхность и **одним проходом** Direct2D: два прохода
        // стоили бы вдвое, рисуя в одну и ту же цель.
        //
        // Отметка `Presented` ставится ПОСЛЕ них: и оверлей, и курсор
        // — часть кадра, и их стоимость обязана попасть в замер, а не
        // спрятаться за отметкой.
        let cursor_draw = match (self.cursor_position, self.cursor.has_shape()) {
            (Some((x, y)), true) => Some(overlay::CursorDraw {
                renderer: &self.cursor,
                x,
                y,
                width: self.presented_size.0 as f32,
                height: self.presented_size.1 as f32,
                // Во сколько раз кадр хоста ужат в окно. Форма курсора
                // приходит в пикселях хоста, поэтому без этого
                // множителя она рисуется в своём родном размере
                // поверх ужатой картинки — и курсор выглядит крупнее
                // всего остального.
                //
                // Берётся ширина: пропорции кадра и окна совпадают
                // (окно создаётся под кадр), а расхождение по высоте
                // из-за обрезки 1088→1080 меньше процента.
                scale: if visible.width > 0 {
                    self.presented_size.0 as f32 / visible.width as f32
                } else {
                    1.0
                },
            }),
            // Позиция без формы или форма без позиции — не повод
            // рисовать: в первом случае нечем, во втором некуда.
            _ => None,
        };

        let need_overlay = self.overlay.is_visible() && !stats.is_empty();
        if need_overlay || cursor_draw.is_some() {
            let surface = self.swapchain.back_buffer_surface()?;
            self.overlay
                .draw_with_cursor(&surface, stats, cursor_draw)?;
        }

        timings.mark(Stage::Presented, self.epoch.stamp_now());
        self.swapchain.present()
    }
}
