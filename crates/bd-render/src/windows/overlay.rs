//! Оверлей статистики поверх видео.
//!
//! # Почему Direct2D, а не webview
//!
//! Оверлей рисуется **в том же D3D11**, что и видео (CLAUDE.md §6.1).
//! Наложить панель из webview поверх видеоокна значило бы вернуть
//! композитинг браузера, ради избавления от которого и заведено
//! отдельное нативное окно.
//!
//! Direct2D умеет рисовать прямо в поверхность нашего swapchain
//! (`CreateDxgiSurfaceRenderTarget`), поэтому текст обходится без
//! промежуточных текстур и без единой копии в системную память.
//!
//! # Почему это часть продукта, а не отладки
//!
//! Задержка — главный критерий проекта, и §10.4 требует, чтобы
//! оверлей со статистикой был доступен **в любой сборке** по горячей
//! клавише. Цифра, которую видно только в консоли разработчика,
//! не поможет ни при диагностике у пользователя, ни при проверке
//! камерой на 240 fps.

use crate::{RenderError, Result};
use std::cell::RefCell;
use windows::core::w;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGISurface;

/// Отступ панели от края окна, в аппаратно-независимых пикселях.
const MARGIN: f32 = 12.0;

/// Внутренний отступ панели.
const PADDING: f32 = 10.0;

/// Размер шрифта. Моноширинный: цифры не должны прыгать при смене
/// значения, иначе оверлей мельтешит и мешает смотреть на видео.
const FONT_SIZE: f32 = 13.0;

/// Высота строки.
const LINE_HEIGHT: f32 = 17.0;

/// Оверлей: панель со статистикой поверх кадра.
///
/// # Порядок полей важен
///
/// Цель вывода D2D держит поверхность swapchain. Она пересоздаётся
/// при каждом изменении размера окна и обязана быть отпущена до
/// `ResizeBuffers` — иначе тот вернёт ошибку (та же причина, что у
/// представления заднего буфера в `swapchain.rs`).
pub struct StatsOverlay {
    /// Цель вывода и кисть, привязанные к текущей поверхности.
    ///
    /// `None` до первой отрисовки и после каждого изменения размера.
    target: RefCell<Option<Target>>,
    format: IDWriteTextFormat,
    d2d: ID2D1Factory,
    /// Виден ли оверлей. Переключается горячей клавишей (§10.4).
    visible: bool,
}

/// Что нужно знать, чтобы нарисовать курсор в проходе оверлея.
///
/// Позиция в долях, а не в пикселях: разрешение хоста и размер окна
/// клиента не совпадают, и пересчёт делается в одном месте — здесь.
pub struct CursorDraw<'a> {
    /// Подготовленная форма курсора.
    pub renderer: &'a super::cursor::CursorRenderer,
    /// Доля по горизонтали, 0.0–1.0.
    pub x: f32,
    /// Доля по вертикали, 0.0–1.0.
    pub y: f32,
    /// Ширина клиентской области в пикселях.
    pub width: f32,
    /// Высота клиентской области в пикселях.
    pub height: f32,
    /// Во сколько раз кадр хоста ужат в окно.
    ///
    /// Форма курсора приходит в пикселях хоста, а рисуется в пикселях
    /// окна. Без множителя курсор выглядит крупнее картинки ровно во
    /// столько раз, во сколько кадр ужат.
    pub scale: f32,
}

/// Цель вывода, привязанная к конкретной поверхности swapchain.
struct Target {
    render_target: ID2D1RenderTarget,
    text: ID2D1SolidColorBrush,
    background: ID2D1SolidColorBrush,
}

impl StatsOverlay {
    /// Создать оверлей.
    pub fn new() -> Result<Self> {
        // SAFETY: тип фабрики задан параметром; опции не нужны.
        // SINGLE_THREADED — весь рендер идёт из одного потока, и
        // многопоточная фабрика брала бы блокировки впустую.
        let d2d: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .map_err(|e| RenderError::SwapChain(format!("D2D1CreateFactory: {e}")))?;

        // SAFETY: тип фабрики задан параметром типа.
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
            .map_err(|e| RenderError::SwapChain(format!("DWriteCreateFactory: {e}")))?;

        // Consolas: моноширинный, есть в любой Windows. Пропорциональный
        // шрифт заставлял бы цифры прыгать по горизонтали при каждом
        // обновлении.
        // SAFETY: строки — константы, живущие всю программу; коллекция
        // шрифтов по умолчанию задаётся как None.
        let format = unsafe {
            dwrite.CreateTextFormat(
                w!("Consolas"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE,
                w!("en-us"),
            )
        }
        .map_err(|e| RenderError::SwapChain(format!("CreateTextFormat: {e}")))?;

        Ok(Self {
            target: RefCell::new(None),
            format,
            d2d,
            visible: true,
        })
    }

    /// Виден ли оверлей.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Переключить видимость.
    ///
    /// Оверлей обязан скрываться: он закрывает часть картинки, а
    /// пользователю нужна не статистика, а удалённый экран.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        tracing::debug!(visible = self.visible, "видимость оверлея переключена");
    }

    /// Отпустить цель вывода.
    ///
    /// Обязательно перед `ResizeBuffers`: пока цель держит поверхность
    /// заднего буфера, изменить размер swapchain нельзя.
    pub fn release_target(&self) {
        *self.target.borrow_mut() = None;
    }

    /// Цель вывода Direct2D для этой поверхности.
    ///
    /// Нужна тем, кто создаёт ресурсы, привязанные к цели, — например,
    /// битмап курсора. Цель одна на оверлей и курсор: у Direct2D
    /// ресурсы принадлежат конкретной цели, и битмап, созданный на
    /// чужой, рисоваться не будет.
    pub fn render_target_for(&self, surface: &IDXGISurface) -> Result<ID2D1RenderTarget> {
        self.ensure_target(surface)?;
        self.target
            .borrow()
            .as_ref()
            .map(|t| t.render_target.clone())
            .ok_or_else(|| RenderError::SwapChain("цель вывода D2D не создана".into()))
    }

    /// Нарисовать панель статистики и курсор одним проходом Direct2D.
    ///
    /// Цель вывода создаётся при первом вызове после изменения
    /// размера и переиспользуется дальше: пересоздавать её каждый
    /// кадр значило бы отдавать время впустую 60 раз в секунду.
    ///
    /// # Почему вместе, а не двумя вызовами
    ///
    /// `BeginDraw`/`EndDraw` — это переключение цели вывода и сброс
    /// команд на GPU. Два прохода на кадр стоили бы вдвое, а рисуют
    /// они в одну и ту же поверхность. К тому же цель вывода одна:
    /// второй `BeginDraw` поверх незакрытого первого — ошибка Direct2D.
    ///
    /// Курсор рисуется **после** панели: он должен быть виден поверх
    /// неё, а не прятаться под статистикой.
    ///
    /// Панель рисуется только когда оверлей включён, а курсор —
    /// всегда: это часть картинки, а не отладочная информация.
    pub fn draw_with_cursor(
        &self,
        surface: &IDXGISurface,
        lines: &[String],
        cursor: Option<CursorDraw<'_>>,
    ) -> Result<()> {
        let draw_panel = self.visible && !lines.is_empty();
        if !draw_panel && cursor.is_none() {
            return Ok(());
        }

        self.ensure_target(surface)?;

        let borrowed = self.target.borrow();
        let Some(target) = borrowed.as_ref() else {
            return Ok(());
        };

        // SAFETY: цель вывода жива; парные BeginDraw/EndDraw.
        unsafe { target.render_target.BeginDraw() };

        if draw_panel {
            self.draw_panel(target, lines);
        }

        if let Some(cursor) = cursor {
            cursor.renderer.draw(
                &target.render_target,
                cursor.x,
                cursor.y,
                cursor.width,
                cursor.height,
                cursor.scale,
            );
        }

        // SAFETY: BeginDraw вызван выше. EndDraw возвращает ошибку
        // отложенно: она относится ко всем вызовам с BeginDraw, а не
        // к последнему.
        unsafe { target.render_target.EndDraw(None, None) }.map_err(|e| RenderError::Platform {
            context: "ID2D1RenderTarget::EndDraw",
            hresult: e.code().0 as u32,
        })?;

        Ok(())
    }

    /// Нарисовать панель статистики. Вызывать между BeginDraw и EndDraw.
    fn draw_panel(&self, target: &Target, lines: &[String]) {
        let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32;
        // Ширина символа Consolas — примерно 0.55 от кегля. Точный
        // замер через IDWriteTextLayout здесь избыточен: панель
        // рисуется на глаз, а не верстается.
        let panel_width = width * FONT_SIZE * 0.55 + PADDING * 2.0;
        let panel_height = lines.len() as f32 * LINE_HEIGHT + PADDING * 2.0;

        // SAFETY: цель вывода жива; вызовы не принимают внешних
        // указателей, кроме описанных ниже структур. BeginDraw/EndDraw
        // вызывает `draw_with_cursor`: панель и курсор рисуются в один
        // проход.
        unsafe {
            let panel = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: MARGIN,
                    top: MARGIN,
                    right: MARGIN + panel_width,
                    bottom: MARGIN + panel_height,
                },
                radiusX: 8.0,
                radiusY: 8.0,
            };
            target
                .render_target
                .FillRoundedRectangle(&panel, &target.background);

            for (index, line) in lines.iter().enumerate() {
                let top = MARGIN + PADDING + index as f32 * LINE_HEIGHT;
                let layout = D2D_RECT_F {
                    left: MARGIN + PADDING,
                    top,
                    right: MARGIN + panel_width - PADDING,
                    bottom: top + LINE_HEIGHT,
                };

                let text: Vec<u16> = line.encode_utf16().collect();
                target.render_target.DrawText(
                    &text,
                    &self.format,
                    &layout,
                    &target.text,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    /// Создать цель вывода, если её ещё нет.
    fn ensure_target(&self, surface: &IDXGISurface) -> Result<()> {
        if self.target.borrow().is_some() {
            return Ok(());
        }

        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                // Кадр уже нарисован и непрозрачен; альфа заднего
                // буфера не используется.
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            // 96 DPI, а НЕ ноль.
            //
            // Ноль означает «взять DPI системы», и тогда Direct2D
            // работает в аппаратно-независимых пикселях: при масштабе
            // Windows 150 % всё нарисованное растягивается в полтора
            // раза. Для панели статистики это было незаметно (она и
            // так «на глаз»), но курсор 32×32 превращался в 48×48 —
            // пользователь сообщил, что курсор слишком большой.
            //
            // 96 DPI — это масштаб 1:1, где одна единица Direct2D
            // равна одному пикселю поверхности. Именно то, что нужно:
            // и курсор, и панель позиционируются в пикселях кадра.
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: Default::default(),
        };

        // SAFETY: поверхность жива; описание живёт до конца вызова.
        let render_target = unsafe { self.d2d.CreateDxgiSurfaceRenderTarget(surface, &props) }
            .map_err(|e| RenderError::SwapChain(format!("CreateDxgiSurfaceRenderTarget: {e}")))?;

        // SAFETY: цель вывода только что создана; цвета живут до
        // конца вызова, свойства кисти не нужны.
        let text =
            unsafe { render_target.CreateSolidColorBrush(&color(0.93, 0.94, 0.96, 1.0), None) }
                .map_err(|e| RenderError::SwapChain(format!("CreateSolidColorBrush: {e}")))?;

        // Полупрозрачная тёмная подложка: по белому тексту на светлом
        // участке экрана иначе ничего не прочитать.
        // SAFETY: см. выше.
        let background =
            unsafe { render_target.CreateSolidColorBrush(&color(0.08, 0.09, 0.11, 0.72), None) }
                .map_err(|e| RenderError::SwapChain(format!("CreateSolidColorBrush: {e}")))?;

        *self.target.borrow_mut() = Some(Target {
            render_target,
            text,
            background,
        });
        Ok(())
    }
}

/// Цвет D2D из компонент.
fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

// SAFETY: все объекты D2D/DWrite созданы как SINGLE_THREADED и
// используются только из потока рендера. Перемещение между потоками
// допустимо, одновременный доступ — нет, что обеспечивает `&mut self`
// в методах владельца.
unsafe impl Send for StatsOverlay {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_components_keep_their_order() {
        // Перепутанные местами компоненты не дают ни ошибки сборки,
        // ни ошибки вызова — только неверный цвет, который легко
        // списать на «так и задумано».
        let c = color(0.1, 0.2, 0.3, 0.4);
        assert_eq!((c.r, c.g, c.b, c.a), (0.1, 0.2, 0.3, 0.4));
    }
}
