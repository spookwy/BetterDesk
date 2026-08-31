//! Захват через DXGI Desktop Duplication.
//!
//! # Правило, определяющее задержку
//!
//! `ReleaseFrame` вызывается **непосредственно перед** следующим
//! `AcquireNextFrame`, а не сразу после обработки кадра. Причина: API
//! накапливает обновления рабочего стола в промежутке между release и
//! acquire. Чем этот промежуток короче, тем свежее полученный кадр
//! (CLAUDE.md §5.1).
//!
//! Отсюда устройство структуры: флаг `frame_acquired` живёт между
//! вызовами, и освобождение происходит в начале следующего `next_frame`.

use super::hresult_to_error;
use crate::{CaptureError, CaptureOutcome, Capturer, Result};
use bd_core::cursor::{CursorPosition, CursorShape, CursorShapeKind};
use bd_core::frame::{FrameInfo, FrameSize, PixelFormat};
use bd_core::metrics::{FrameTimings, Stage};
use bd_core::time::Epoch;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

use super::device::D3dDevice;

/// Захваченный кадр: GPU-текстура и метаданные.
///
/// Текстура **не копируется в системную память** (CLAUDE.md §4.2.3).
/// Она принадлежит дупликации и действительна до следующего
/// `ReleaseFrame`, то есть до следующего вызова
/// [`Capturer::next_frame`](crate::Capturer::next_frame).
pub struct DxgiFrame {
    texture: ID3D11Texture2D,
    info: FrameInfo,
}

impl DxgiFrame {
    /// GPU-текстура кадра в формате BGRA8.
    ///
    /// Отдаётся по ссылке: владение остаётся у дупликации, а время
    /// жизни ограничено следующим захватом.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// Метаданные кадра, включая тайминги.
    pub fn info(&self) -> &FrameInfo {
        &self.info
    }

    /// Изменяемая ссылка на метаданные — чтобы следующие стадии
    /// пайплайна проставляли свои отметки времени.
    pub fn info_mut(&mut self) -> &mut FrameInfo {
        &mut self.info
    }
}

impl std::fmt::Debug for DxgiFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiFrame")
            .field("size", &self.info.size)
            .field("changed", &self.info.content_changed)
            .finish_non_exhaustive()
    }
}

/// Захват экрана через DXGI Desktop Duplication.
pub struct DxgiCapturer {
    device: D3dDevice,
    output: IDXGIOutput1,
    /// Дупликация. `None` только в момент восстановления: старую
    /// надо освободить до создания новой, иначе DXGI откажет
    /// (см. `recover`). В остальное время всегда `Some`.
    duplication: Option<IDXGIOutputDuplication>,
    size: FrameSize,
    monitor_index: u32,
    epoch: Epoch,
    sequence: u64,
    /// Удерживается ли сейчас кадр, который нужно освободить.
    ///
    /// `AcquireNextFrame` вернёт `DXGI_ERROR_INVALID_CALL`, если
    /// предыдущий кадр не освобождён, поэтому состояние отслеживается явно.
    frame_acquired: bool,
    /// Последнее состояние курсора.
    ///
    /// Копится здесь, а не отдаётся с кадром: курсор живёт своей
    /// жизнью. DXGI сообщает о его перемещении даже тогда, когда
    /// содержимое экрана не менялось (`LastPresentTime == 0`), и такие
    /// уведомления — большинство при обычной работе (находка 7).
    /// Отдавать курсор только вместе с кадром значило бы терять
    /// движение мыши на статичном экране.
    cursor: CursorState,
    /// Номер следующей формы курсора.
    ///
    /// Растёт при каждой смене формы. Клиент кеширует формы по этому
    /// номеру, поэтому переиспользовать номера нельзя.
    next_shape_id: u32,
    /// Буфер под форму курсора.
    ///
    /// Переиспользуется между вызовами: `GetFramePointerShape`
    /// вызывается на каждой смене формы, а аллокация в горячем пути
    /// не нужна.
    shape_buffer: Vec<u8>,
}

/// Состояние курсора между кадрами.
#[derive(Debug, Clone)]
pub struct CursorState {
    /// Позиция и видимость.
    pub position: CursorPosition,
    /// Новая форма, если она изменилась с прошлого запроса.
    ///
    /// Забирается «до востребования»: форма весит килобайты, и
    /// отдавать её на каждом кадре было бы расточительно. `None`
    /// означает «форма прежняя», а не «формы нет».
    pub new_shape: Option<CursorShape>,
    /// Менялось ли что-нибудь с прошлого запроса.
    pub changed: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            position: CursorPosition {
                position: bd_core::input::MousePosition::new(0.5, 0.5),
                visible: false,
                shape_id: 0,
            },
            new_shape: None,
            changed: false,
        }
    }
}

impl DxgiCapturer {
    /// Начать захват монитора с указанным индексом.
    ///
    /// Индексы соответствуют [`MonitorInfo::index`](crate::MonitorInfo::index)
    /// из [`enumerate_monitors`](super::enumerate_monitors).
    pub fn new(monitor_index: u32, epoch: Epoch) -> Result<Self> {
        let (device, output) = D3dDevice::for_monitor(monitor_index)?;
        let duplication = create_duplication(&device, &output)?;
        let size = duplication_size(&duplication);

        tracing::info!(
            monitor_index,
            width = size.width,
            height = size.height,
            "DXGI-дупликация запущена"
        );

        Ok(Self {
            device,
            output,
            duplication: Some(duplication),
            size,
            monitor_index,
            epoch,
            sequence: 0,
            frame_acquired: false,
            cursor: CursorState::default(),
            next_shape_id: 0,
            shape_buffer: Vec::new(),
        })
    }

    /// Указатель на D3D11-устройство, на котором работает захват.
    ///
    /// Нужен энкодеру: он обязан работать на том же адаптере, иначе
    /// текстуру пришлось бы копировать между GPU и zero-copy
    /// потерялся бы (CLAUDE.md §4.2.3).
    ///
    /// Указатель действителен, пока жив капчурер.
    pub fn device_ptr(&self) -> *mut std::ffi::c_void {
        self.device.device().as_raw()
    }

    /// D3D11-устройство захвата.
    ///
    /// То же устройство, что и [`device_ptr`](Self::device_ptr), но
    /// типизированно. Нужно тем потребителям, чьи API принимают
    /// интерфейс, а не сырой указатель — например, декодеру
    /// Media Foundation. Предпочитать этот метод: сырой указатель
    /// нужен только там, где его требует C-ABI (CLAUDE.md §4.3.3).
    pub fn device(&self) -> &ID3D11Device {
        self.device.device()
    }

    /// Текущая дупликация.
    ///
    /// `None` возможен только внутри [`Capturer::recover`], между
    /// освобождением старой дупликации и созданием новой. Наружу
    /// такое состояние не выходит, поэтому здесь это ошибка, а не
    /// штатный случай.
    fn duplication(&self) -> Result<&IDXGIOutputDuplication> {
        self.duplication.as_ref().ok_or(CaptureError::Platform {
            context: "дупликация не создана",
            code: 0,
        })
    }

    /// Освободить удерживаемый кадр, если он есть.
    ///
    /// Идемпотентна: повторный вызов безопасен.
    fn release_held_frame(&mut self) {
        if !self.frame_acquired {
            return;
        }
        let Some(duplication) = self.duplication.as_ref() else {
            // Дупликации нет — освобождать нечего и не через что.
            self.frame_acquired = false;
            return;
        };

        // SAFETY: `duplication` жив; флаг `frame_acquired` гарантирует,
        // что кадр был получен и ещё не освобождён — только в этом
        // случае ReleaseFrame корректен.
        let result = unsafe { duplication.ReleaseFrame() };
        self.frame_acquired = false;

        if let Err(err) = result {
            // Потеря доступа при освобождении — штатная ситуация
            // (например, сменилось разрешение). Восстановление
            // произойдёт на следующем AcquireNextFrame.
            tracing::trace!(?err, "ReleaseFrame вернул ошибку");
        }
    }
}

impl Capturer for DxgiCapturer {
    type Frame = DxgiFrame;

    fn next_frame(&mut self, timeout: Duration) -> Result<CaptureOutcome<DxgiFrame>> {
        // Освобождаем прошлый кадр непосредственно перед новым запросом,
        // а не после обработки — см. заметку модуля.
        self.release_held_frame();

        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: `duplication` жив; выходные параметры — валидные
        // локальные переменные подходящих типов. Предыдущий кадр
        // освобождён выше, поэтому DXGI_ERROR_INVALID_CALL исключён.
        let hr = unsafe {
            self.duplication()?
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };

        if let Err(err) = hr {
            if err.code() == DXGI_ERROR_WAIT_TIMEOUT {
                // Экран не изменился. Это не ошибка: такой кадр не нужно
                // ни кодировать, ни отправлять (CLAUDE.md §5.1).
                return Ok(CaptureOutcome::Timeout);
            }
            return Err(hresult_to_error(err.code(), "AcquireNextFrame"));
        }

        self.frame_acquired = true;
        let captured_at = self.epoch.stamp_now();

        let resource = resource.ok_or(CaptureError::Platform {
            context: "AcquireNextFrame вернул пустой ресурс",
            code: 0,
        })?;

        // SAFETY: `resource` получен из успешного AcquireNextFrame и не
        // освобождён. Ресурсы Desktop Duplication всегда реализуют
        // ID3D11Texture2D — это гарантировано документацией DXGI.
        let texture: ID3D11Texture2D = resource.cast().map_err(|e| CaptureError::Platform {
            context: "приведение ресурса к ID3D11Texture2D",
            code: e.code().0 as u32,
        })?;

        // Курсор обновляется до проверки содержимого: он меняется
        // именно тогда, когда экран не менялся, и пропускать эти
        // уведомления значило бы терять движение мыши на статичной
        // картинке (находка 7).
        self.update_cursor(&frame_info);

        // LastPresentTime == 0 означает, что содержимое рабочего стола
        // не менялось — обновилась только позиция или форма курсора.
        // Такой кадр перекодировать не нужно (см. документацию
        // DXGI_OUTDUPL_FRAME_INFO).
        let content_changed = frame_info.LastPresentTime != 0;

        let mut timings = FrameTimings::new(self.sequence);
        timings.mark(Stage::Captured, captured_at);
        self.sequence += 1;

        let mut info = FrameInfo::new(self.size, PixelFormat::Bgra8, timings);
        if !content_changed {
            info = info.unchanged();
        }

        Ok(CaptureOutcome::Frame(DxgiFrame { texture, info }))
    }

    fn size(&self) -> FrameSize {
        self.size
    }

    fn recover(&mut self) -> Result<()> {
        // Курсор после восстановления неизвестен: форма могла
        // смениться, пока дупликации не было. Сбрасывается видимость,
        // но не номер формы — иначе клиент получил бы форму со старым
        // номером и нарисовал прежнюю картинку.
        self.cursor.position.visible = false;
        self.cursor.changed = true;
        self.recover_duplication()
    }
}

impl DxgiCapturer {
    /// Обновить состояние курсора по данным кадра.
    ///
    /// Вызывается на каждом кадре, включая те, где менялся только
    /// курсор. Форма запрашивается лишь при её смене: `GetFramePointerShape`
    /// копирует килобайты, и делать это на каждом движении мыши
    /// незачем.
    fn update_cursor(&mut self, frame_info: &DXGI_OUTDUPL_FRAME_INFO) {
        // LastMouseUpdateTime == 0 означает, что курсор не трогали.
        // Ни позиция, ни форма не изменились — выходим, сохранив
        // прежнее состояние.
        if frame_info.LastMouseUpdateTime == 0 {
            return;
        }

        let pointer = &frame_info.PointerPosition;
        let visible = pointer.Visible.as_bool();

        // Позиция приходит в пикселях монитора и нормализуется здесь:
        // клиент не знает разрешения хоста, а оно может смениться.
        //
        // Обновляется только при видимом курсоре: у скрытого DXGI
        // оставляет в структуре последние координаты, и принять их
        // за новые значило бы дёргать курсор при каждом скрытии.
        if visible {
            self.cursor.position.position = bd_core::input::MousePosition::from_pixels(
                pointer.Position.x,
                pointer.Position.y,
                self.size.width,
                self.size.height,
            );
        }
        self.cursor.position.visible = visible;
        self.cursor.changed = true;

        // Форма пришла только если PointerShapeBufferSize > 0.
        if frame_info.PointerShapeBufferSize == 0 {
            return;
        }

        // Размер приходит от DXGI, но проверяется всё равно: буфер
        // выделяется по этому числу.
        let required = frame_info.PointerShapeBufferSize as usize;
        const MAX_SHAPE_BYTES: usize = 256 * 256 * 4 * 2;
        if required > MAX_SHAPE_BYTES {
            tracing::warn!(required, "форма курсора неправдоподобно велика, пропущена");
            return;
        }

        self.shape_buffer.resize(required, 0);
        let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut written = 0u32;

        let Some(duplication) = self.duplication.as_ref() else {
            return;
        };

        // SAFETY: буфер размером `required` байт выделен выше и жив до
        // конца вызова; `written` и `shape_info` — живые локальные
        // переменные. Размер буфера передаётся тот же, что его
        // фактическая длина, поэтому выхода за границы быть не может.
        let hr = unsafe {
            duplication.GetFramePointerShape(
                required as u32,
                self.shape_buffer.as_mut_ptr().cast(),
                &mut written,
                &mut shape_info,
            )
        };

        if let Err(err) = hr {
            // Не повод прерывать захват: курсор останется прежним.
            // Форма придёт со следующим её изменением.
            tracing::debug!(?err, "не удалось получить форму курсора");
            return;
        }

        let kind = match DXGI_OUTDUPL_POINTER_SHAPE_TYPE(shape_info.Type as i32) {
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME => CursorShapeKind::Monochrome,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR => CursorShapeKind::Color,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR => CursorShapeKind::MaskedColor,
            other => {
                // Неизвестный вид рисовать нечем. Молча подставить
                // цветной значило бы показать мусор вместо курсора.
                tracing::debug!(kind = other.0, "неизвестный вид формы курсора");
                return;
            }
        };

        // Монохромный курсор несёт две маски подряд, поэтому его
        // высота вдвое меньше высоты буфера. Не поделить пополам —
        // значит объявить курсор вдвое длиннее, чем он есть.
        let height = match kind {
            CursorShapeKind::Monochrome => shape_info.Height / 2,
            _ => shape_info.Height,
        };

        // Диагностика формы курсора: вид, размер и точка привязки.
        //
        // Оставлено намеренно, хотя в обычном прогоне не видно
        // (уровень `debug`). Причина: предположения об этих числах
        // оказывались неверными трижды — DPI (находка 36а), инверсия
        // (36б) и вид формы, который считался монохромным, а на живой
        // машине не встречается вовсе. Вид курсора глазами не
        // определить, а без него разбор жалоб «курсор не такой»
        // начинается с гадания.
        //
        // Включается так:
        //   $env:RUST_LOG="bd_capture=debug"
        tracing::debug!(
            kind = ?kind,
            width = shape_info.Width,
            height,
            buffer_height = shape_info.Height,
            hotspot_x = shape_info.HotSpot.x,
            hotspot_y = shape_info.HotSpot.y,
            "форма курсора"
        );

        // Карта пикселей формы в ASCII.
        //
        // # Зачем это осталось в коде
        //
        // Вид курсора нельзя определить ни глазом, ни тестом: тест
        // проверяет то, что мы **предположили** о формате, а глаз
        // видит результат, а не причину. Из-за этого судьба
        // инвертирующих пикселей менялась трижды — чёрные (находка
        // 36б, дало чёрный квадрат), прозрачные (курсор пропадал над
        // текстом), белые (дало белый квадрат) — и каждый раз правка
        // делалась по догадке.
        //
        // Вопрос закрылся за один прогон, как только форма была
        // напечатана: маска у I-beam стоит на всей площади 48×48, а
        // форму несёт яркость. Ни одна из трёх догадок этого не
        // учитывала.
        //
        // Поэтому дамп остаётся: следующая жалоба на курсор должна
        // начинаться с него, а не с гипотезы.
        //
        // Уровень `trace`, а не `debug`: это 48 строк на каждую смену
        // формы. Включается так:
        //   $env:RUST_LOG="bd_capture::windows::dxgi=trace"
        if tracing::enabled!(tracing::Level::TRACE)
            && matches!(kind, CursorShapeKind::MaskedColor | CursorShapeKind::Color)
        {
            let w = (shape_info.Width as usize).min(64);
            let h = (height as usize).min(64);
            let pitch = shape_info.Pitch as usize;
            let mut map = String::with_capacity((w + 1) * h);
            for y in 0..h {
                for x in 0..w {
                    let src = y * pitch + x * 4;
                    if src + 3 >= self.shape_buffer.len() {
                        break;
                    }
                    let bright = self.shape_buffer[src] as u32
                        + self.shape_buffer[src + 1] as u32
                        + self.shape_buffer[src + 2] as u32;
                    let mask = self.shape_buffer[src + 3];
                    // Маска 0 — обычный пиксель: 'W' светлый, 'K'
                    // тёмный. Маска 255 — инвертирующий: '#' светлый
                    // (форма), '.' тёмный (фон).
                    map.push(match (mask, bright > 380) {
                        (0, true) => 'W',
                        (0, false) => 'K',
                        (255, true) => '#',
                        (255, false) => '.',
                        _ => '?',
                    });
                }
                map.push('\n');
            }
            tracing::trace!(kind = ?kind, "карта формы курсора:\n{map}");
        }

        self.next_shape_id = self.next_shape_id.wrapping_add(1);
        let shape = CursorShape {
            id: self.next_shape_id,
            kind,
            width: shape_info.Width,
            height,
            pitch: shape_info.Pitch,
            hotspot_x: shape_info.HotSpot.x.max(0) as u32,
            hotspot_y: shape_info.HotSpot.y.max(0) as u32,
            pixels: self.shape_buffer[..written as usize].to_vec(),
        };

        // Согласованность проверяется до передачи наружу: несогласованная
        // форма всё равно была бы отвергнута на приёме, и лучше узнать
        // об этом здесь, где видно, что именно пришло от DXGI.
        if !shape.is_consistent() {
            tracing::warn!(
                width = shape.width,
                height = shape.height,
                pitch = shape.pitch,
                bytes = shape.pixels.len(),
                "форма курсора несогласованна, пропущена"
            );
            return;
        }

        self.cursor.position.shape_id = shape.id;
        self.cursor.new_shape = Some(shape);
    }

    /// Забрать состояние курсора, если оно менялось.
    ///
    /// «До востребования»: возвращает `Some` один раз на изменение и
    /// сбрасывает признак. Так позиция не шлётся повторно, пока мышь
    /// стоит на месте, а новая форма отдаётся ровно один раз.
    pub fn take_cursor(&mut self) -> Option<CursorState> {
        if !self.cursor.changed {
            return None;
        }
        self.cursor.changed = false;
        Some(CursorState {
            position: self.cursor.position,
            // Форма забирается: повторно её слать не нужно, клиент
            // кеширует по номеру.
            new_shape: self.cursor.new_shape.take(),
            changed: true,
        })
    }

    /// Пересоздать дупликацию.
    fn recover_duplication(&mut self) -> Result<()> {
        tracing::info!(monitor = self.monitor_index, "пересоздание дупликации");

        // Освобождать удерживаемый кадр через потерянный интерфейс
        // бессмысленно — просто сбрасываем флаг.
        self.frame_acquired = false;

        // Устройство НЕ пересоздаётся. `ACCESS_LOST` означает потерю
        // дупликации (сменилось разрешение, всплыл UAC, перехватил
        // другой процесс), а не потерю устройства D3D11 — то был бы
        // `DEVICE_REMOVED`.
        //
        // Разница принципиальна: на устройстве захвата работают
        // энкодер, декодер и окно вывода. Подменив его здесь, мы
        // оставили бы их всех с мёртвым указателем — и следующий же
        // кадр ушёл бы в никуда. Пересоздание устройства — это
        // пересоздание всего стека, и решать это должен тот, кто им
        // владеет, а не капчурер.
        //
        // Старая дупликация освобождается ДО создания новой. DXGI
        // разрешает одну дупликацию на выход в пределах процесса, и
        // пока живёт прежняя, `DuplicateOutput` отвечает
        // `E_INVALIDARG` — то есть восстановление не срабатывает
        // вовсе. Присваивание полю не годится: оно освободит старое
        // значение только после того, как новое уже создано.
        let output = super::device::output_for_monitor(self.monitor_index)?;
        self.duplication = None;

        let duplication = create_duplication(&self.device, &output)?;
        let size = duplication_size(&duplication);

        if size != self.size {
            // Смена разрешения — самая частая причина ACCESS_LOST.
            // Энкодер настроен на прежний размер, и вызывающий обязан
            // это заметить: молча продолжать нельзя.
            tracing::warn!(
                from = format!("{}x{}", self.size.width, self.size.height),
                to = format!("{}x{}", size.width, size.height),
                "разрешение монитора изменилось — стек кодеков надо пересоздать"
            );
        }

        self.size = size;
        self.output = output;
        self.duplication = Some(duplication);

        tracing::info!(
            width = self.size.width,
            height = self.size.height,
            "дупликация восстановлена"
        );
        Ok(())
    }
}

impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        // Освобождение удерживаемого кадра до разрушения дупликации —
        // иначе драйвер может пожаловаться на незакрытый ресурс.
        self.release_held_frame();
    }
}

/// Создать дупликацию для выхода.
fn create_duplication(device: &D3dDevice, output: &IDXGIOutput1) -> Result<IDXGIOutputDuplication> {
    // SAFETY: `output` и устройство живы. DuplicateOutput требует,
    // чтобы устройство было создано на том же адаптере, что и выход —
    // это обеспечено в D3dDevice::for_monitor.
    unsafe { output.DuplicateOutput(device.device()) }
        .map_err(|e| hresult_to_error(e.code(), "DuplicateOutput"))
}

/// Прочитать разрешение из описания дупликации.
fn duplication_size(duplication: &IDXGIOutputDuplication) -> FrameSize {
    // SAFETY: `duplication` жив; GetDesc возвращает описание по значению,
    // не принимает входных указателей и не может завершиться ошибкой.
    let desc = unsafe { duplication.GetDesc() };
    FrameSize::new(desc.ModeDesc.Width, desc.ModeDesc.Height)
}
