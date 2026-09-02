//! Нативное окно Win32 для вывода видео.
//!
//! # Почему своё окно, а не webview
//!
//! Видео никогда не рендерится в webview (CLAUDE.md §6.1): композитинг
//! браузера добавляет 10–20 мс и отбирает контроль над очередью кадров.
//! Окно сессии — обычное Win32-окно со своим циклом сообщений, и Tauri
//! про него не знает.
//!
//! # Почему цикл не блокирующий
//!
//! `GetMessage` блокируется до прихода сообщения — а кадры приходят не
//! от оконной очереди, а от декодера. Поэтому насос сообщений
//! неблокирующий (`PeekMessage`), и его прокачивает тот же поток,
//! который рисует. Иначе окно «зависало» бы между кадрами.

use crate::{RenderError, Result};
use std::cell::Cell;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F9;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, PeekMessageW, PostQuitMessage, RegisterClassExW, SetCursor, ShowWindow,
    TranslateMessage, CS_HREDRAW, CS_OWNDC, CS_VREDRAW, CW_USEDEFAULT, HCURSOR, HTCLIENT, MSG,
    PM_REMOVE, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WNDCLASSEXW, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
};

/// Имя оконного класса. Регистрируется один раз на процесс.
const CLASS_NAME: PCWSTR = w!("BetterDeskSession");

thread_local! {
    /// Зарегистрирован ли класс в этом потоке.
    ///
    /// `RegisterClassExW` на уже зарегистрированное имя возвращает
    /// ошибку. Класс привязан к процессу, но окна мы создаём из
    /// потока рендера, поэтому флага в потоке достаточно.
    static CLASS_REGISTERED: Cell<bool> = const { Cell::new(false) };

    /// Попросил ли пользователь закрыть окно.
    ///
    /// Оконная процедура вызывается системой и не имеет доступа к
    /// нашим структурам, поэтому флаг живёт в потоке. Альтернатива —
    /// протащить указатель на состояние через `GWLP_USERDATA`, но
    /// это лишний `unsafe` ради одного булева значения.
    static CLOSE_REQUESTED: Cell<bool> = const { Cell::new(false) };

    /// Нажата ли клавиша переключения оверлея с прошлой проверки.
    ///
    /// Считается «до востребования»: цикл рендера забирает флаг и
    /// сбрасывает его. Так нажатие не теряется, даже если между
    /// кадрами их было несколько.
    static OVERLAY_TOGGLE: Cell<bool> = const { Cell::new(false) };

    /// Очередь сырых сообщений ввода.
    ///
    /// **Сырых, а не разобранных.** `bd-render` не знает про
    /// `bd-input`: обратная зависимость запрещена (CLAUDE.md §4.2.5),
    /// и окно вывода не должно понимать формат событий ввода. Поэтому
    /// сюда складывается то, что дала система, а переводит их в
    /// события тот, кто видит оба крейта.
    ///
    /// Очередь, а не флаг (как у оверлея): лишнее нажатие F9
    /// безвредно, а потерянное отпускание клавиши — это залипание
    /// навсегда.
    static INPUT_MESSAGES: std::cell::RefCell<std::collections::VecDeque<RawInputMessage>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };

    /// Сколько сообщений вытеснено переполнением очереди.
    static INPUT_DROPPED: Cell<u64> = const { Cell::new(0) };
}

/// Предел длины очереди сообщений ввода.
///
/// Очередь наполняет система, опустошает цикл клиента. Если цикл встал
/// (пересоздание кодеков, долгий кадр), сообщения продолжают идти — без
/// предела они съели бы память. При переполнении вытесняется самое
/// старое: устаревшее движение мыши не нужно никому.
const MAX_INPUT_QUEUE: usize = 256;

/// Сырое сообщение ввода из оконной процедуры.
///
/// Намеренно повторяет форму сообщения Win32, а не описывает событие:
/// смысл ему придаёт слой выше (см. [`INPUT_MESSAGES`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawInputMessage {
    /// Код сообщения (`WM_*`).
    pub message: u32,
    /// `wParam` как есть.
    pub wparam: usize,
    /// `lParam` как есть.
    pub lparam: isize,
}

/// Окно сессии.
///
/// RAII: окно уничтожается вместе со значением (CLAUDE.md §4.3.4).
pub struct SessionWindow {
    hwnd: HWND,
}

impl SessionWindow {
    /// Создать окно с клиентской областью заданного размера.
    ///
    /// Размер задаётся именно для *клиентской* области: рамка и
    /// заголовок добавляются сверху. Иначе картинка масштабировалась
    /// бы на пару десятков пикселей и выглядела бы мыльной.
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self> {
        // SAFETY: NULL означает модуль текущего процесса — допустимый
        // аргумент по документации GetModuleHandleW.
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }
            .map_err(|e| RenderError::WindowCreation(format!("GetModuleHandleW: {e}")))?
            .into();

        register_class(instance)?;

        // Пересчёт размера окна из размера клиентской области.
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        // SAFETY: `rect` — живая локальная переменная; стиль тот же,
        // с которым окно создаётся ниже.
        let _ = unsafe { AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false) };

        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: класс зарегистрирован выше; строки завершены нулём и
        // живут до конца вызова; параметры позиции и размера — числа.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW,
                CLASS_NAME,
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|e| RenderError::WindowCreation(format!("CreateWindowExW: {e}")))?;

        CLOSE_REQUESTED.with(|f| f.set(false));

        // SAFETY: окно только что создано.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };

        tracing::info!(width, height, "окно сессии создано");
        Ok(Self { hwnd })
    }

    /// Хендл окна — для создания swapchain.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Текущий размер клиентской области.
    ///
    /// Может отличаться от запрошенного: пользователь тянет за угол,
    /// система масштабирует по DPI. Swapchain обязан следовать за этим
    /// размером, иначе картинка будет растянута.
    pub fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        // SAFETY: окно живо, пока жив `self`; `rect` — живая локальная
        // переменная, которую заполняет вызов.
        if unsafe { GetClientRect(self.hwnd, &mut rect) }.is_err() {
            return (0, 0);
        }
        (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    }

    /// Прокачать очередь сообщений и сказать, жив ли ещё цикл.
    ///
    /// Возвращает `false`, когда пользователь закрыл окно. Вызывать
    /// каждый кадр: без этого окно перестаёт отвечать, и Windows
    /// рисует поверх него белый прямоугольник «не отвечает».
    pub fn pump_messages(&self) -> bool {
        let mut msg = MSG::default();

        // SAFETY: `msg` — живая локальная переменная. PM_REMOVE
        // забирает сообщение из очереди; фильтры нулевые — берём всё.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                return false;
            }
            // SAFETY: `msg` заполнена PeekMessageW.
            let _ = unsafe { TranslateMessage(&msg) };
            // SAFETY: то же самое.
            unsafe { DispatchMessageW(&msg) };
        }

        !CLOSE_REQUESTED.with(|f| f.get())
    }

    /// Забрать флаг нажатия горячей клавиши оверлея.
    ///
    /// Возвращает `true` один раз на нажатие: флаг сбрасывается
    /// чтением. Иначе оверлей мигал бы всё время, пока клавиша нажата.
    pub fn take_overlay_toggle(&self) -> bool {
        OVERLAY_TOGGLE.with(|f| f.replace(false))
    }

    /// Забрать накопленные сообщения ввода.
    ///
    /// Очередь опустошается: сообщение выдаётся ровно один раз.
    /// Вызывать каждый кадр — иначе очередь переполнится и начнёт
    /// терять ввод (см. [`Self::input_dropped`]).
    pub fn drain_input(&self, out: &mut Vec<RawInputMessage>) {
        INPUT_MESSAGES.with(|q| out.extend(q.borrow_mut().drain(..)));
    }

    /// Сколько сообщений ввода потеряно из-за переполнения очереди.
    ///
    /// Ненулевое значение означает, что цикл не успевает за вводом.
    /// Это диагностика, а не ошибка: без неё потеря ввода выглядит
    /// как «мышь дёргается» без объяснения причины.
    pub fn input_dropped(&self) -> u64 {
        INPUT_DROPPED.with(|d| d.get())
    }
}

/// Положить сообщение ввода в очередь.
///
/// Вызывается из оконной процедуры, поэтому не может ни паниковать,
/// ни блокироваться: система вызывает её в своём контексте.
fn push_input(message: RawInputMessage) {
    INPUT_MESSAGES.with(|q| {
        let mut q = q.borrow_mut();
        if q.len() >= MAX_INPUT_QUEUE {
            // Вытесняется самое старое: свежий ввод ценнее
            // устаревшего — та же логика, что у видеокадров (§5.3).
            q.pop_front();
            INPUT_DROPPED.with(|d| d.set(d.get() + 1));
        }
        q.push_back(message);
    });
}

impl Drop for SessionWindow {
    fn drop(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        // SAFETY: окно создано в этом же типе и ещё не уничтожено —
        // повторный Drop невозможен, значение потребляется.
        if let Err(e) = unsafe { DestroyWindow(self.hwnd) } {
            tracing::warn!(error = %e, "DestroyWindow вернул ошибку");
        }
    }
}

/// Зарегистрировать оконный класс, если это ещё не сделано.
fn register_class(instance: HINSTANCE) -> Result<()> {
    if CLASS_REGISTERED.with(|f| f.get()) {
        return Ok(());
    }

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        // CS_OWNDC: окно получает собственный контекст устройства —
        // рекомендация для окон, которые рисует не GDI.
        style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        // Курсора у класса НЕТ — и это не упущение.
        //
        // Здесь стоял `IDC_ARROW`, и над видео оказывалось **два**
        // курсора сразу: свой, который рисует Windows, и присланный
        // хостом, который рисуем мы. Человек за клиентом видел, как
        // чужая стрелка тянется за его собственной.
        //
        // Убирать надо именно свой, а не чужой, и порядок здесь
        // обратный интуиции. Свой курсор мгновенный и оттого кажется
        // «правильным» — но он показывает, где мышь у НАС, а не где
        // она у хоста. Управляем же мы хостом: значимо только то,
        // куда доехало нажатие. Оставь мы свой, человек целился бы
        // им и промахивался ровно на задержку канала.
        //
        // Чужой курсор вдобавок несёт форму (стрелка, палочка, рука)
        // — то есть показывает, что под ним на ТОЙ стороне. Свой об
        // этом не знает ничего.
        //
        // Побочно исчезает и жалоба на «двойной курсор»: задержка
        // никуда не делась, но глазу больше не с чем её сравнивать —
        // раньше расхождение двух стрелок делало её заметной там,
        // где сама по себе она не мешала.
        //
        // `HCURSOR(null)` означает «класс курсора не задаёт», и
        // Windows не рисует в клиентской области ничего. Рамка и
        // заголовок при этом свой курсор сохраняют — их рисует не
        // класс, и тянуть окно за край по-прежнему можно.
        hCursor: HCURSOR(std::ptr::null_mut()),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };

    // SAFETY: структура заполнена и живёт до конца вызова; размер
    // указан в cbSize, как того требует API.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return Err(RenderError::WindowCreation(
            "RegisterClassExW вернул 0".into(),
        ));
    }

    CLASS_REGISTERED.with(|f| f.set(true));
    Ok(())
}

/// Оконная процедура.
///
/// Намеренно минимальна: всё, что можно сделать в цикле рендера,
/// делается там. Здесь только то, что система спрашивает синхронно.
extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE | WM_DESTROY => {
            CLOSE_REQUESTED.with(|f| f.set(true));
            if msg == WM_DESTROY {
                // SAFETY: вызов без указателей; ставит WM_QUIT в очередь.
                unsafe { PostQuitMessage(0) };
            }
            LRESULT(0)
        }
        // Изменение размера обрабатывает цикл рендера: он сравнивает
        // размер клиентской области с размером swapchain. Делать это
        // здесь значило бы трогать D3D из оконной процедуры, которую
        // система может вызвать в неудобный момент.
        WM_SIZE => LRESULT(0),
        // Свой курсор над видео не рисуется.
        //
        // # Почему пустого hCursor у класса НЕ ХВАТИЛО
        //
        // Живой прогон Москва — Франция показал два курсора и после
        // того, как класс перестал задавать `IDC_ARROW`. Причина в
        // том, что `hCursor` класса — лишь значение по умолчанию:
        // при каждом движении мыши система шлёт `WM_SETCURSOR`, и
        // `DefWindowProcW` в ответ ставит стандартную стрелку. То
        // есть курсор возвращался на каждом же движении.
        //
        // Убрать его можно только ответив на это сообщение самим:
        // `SetCursor(None)` прячет указатель, `LRESULT(1)` говорит
        // системе, что мы разобрались и звать `DefWindowProc` не
        // надо.
        //
        // # Почему только над клиентской областью
        //
        // Младшее слово `lparam` — код зоны попадания. Прячем курсор
        // только над `HTCLIENT`, то есть над самой картинкой. На
        // рамке и заголовке он обязан остаться: иначе окно нельзя
        // будет ни потянуть за край, ни закрыть — человек попросту
        // не увидит, куда целится.
        WM_SETCURSOR if (lparam.0 as u32 & 0xFFFF) == HTCLIENT => {
            // SAFETY: `None` — документированный способ убрать
            // указатель; чужих указателей вызов не принимает.
            unsafe { SetCursor(None) };
            LRESULT(1)
        }
        // F9 — переключение оверлея статистики (CLAUDE.md §10.4).
        // Обрабатывается только факт нажатия; сам оверлей переключает
        // цикл рендера, у которого есть доступ к состоянию.
        //
        // Стоит ПЕРЕД веткой ввода: F9 — наша горячая клавиша, и
        // отправлять её на хост не нужно. Иначе нажатие переключало бы
        // оверлей и здесь, и там.
        WM_KEYDOWN if wparam.0 as u32 == VK_F9.0 as u32 => {
            OVERLAY_TOGGLE.with(|f| f.set(true));
            LRESULT(0)
        }
        // Ввод: клавиатура и мышь.
        //
        // `WM_SYSKEYDOWN`/`WM_SYSKEYUP` обязательны наравне с обычными:
        // именно ими приходит Alt и сочетания с ним. Без них Alt+Tab
        // на хосте не сработает, а сам Alt останется зажатым — то
        // самое залипание, которое запрещает критерий этапа 2.
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_MOUSEMOVE | WM_LBUTTONDOWN
        | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP
        | WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            push_input(RawInputMessage {
                message: msg,
                wparam: wparam.0,
                lparam: lparam.0,
            });

            // Системные клавиши всё же отдаются системе: иначе
            // перестанет работать закрытие окна по Alt+F4, а окно
            // без штатного выхода — это плохо.
            if matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                // SAFETY: параметры переданы системой как есть.
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            LRESULT(0)
        }
        // Потеря фокуса. Клавиши, зажатые в этот момент, отпустить уже
        // не удастся: их отпускание получит другое окно. Сообщаем об
        // этом слою выше, чтобы он отпустил всё на хосте.
        WM_KILLFOCUS => {
            push_input(RawInputMessage {
                message: msg,
                wparam: 0,
                lparam: 0,
            });
            LRESULT(0)
        }
        // SAFETY: параметры переданы системой как есть.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
