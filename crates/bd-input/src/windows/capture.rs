//! Захват ввода в окне клиента.
//!
//! # Как это устроено
//!
//! Оконная процедура вызывается системой в произвольный момент и не
//! имеет доступа к состоянию сессии. Поэтому она только **складывает
//! события в очередь**, а разбирает их цикл клиента — тот же приём,
//! что уже используется для горячей клавиши оверлея в `bd-render`.
//!
//! Очередь, а не флаг: нажатия терять нельзя. Флаг годится для
//! «показать оверлей» (лишнее нажатие ничего не портит), но потерянное
//! отпускание клавиши — это залипание навсегда (см. [`crate::tracker`]).
//!
//! # Почему события переводятся в HID здесь
//!
//! Окно получает виртуальные коды и скан-коды Windows. В сеть уходит
//! платформенно-независимый [`KeyCode`], иначе хост на другой ОС (или
//! с другой раскладкой) не понял бы, что нажато. Перевод — здесь,
//! на границе платформы.

use bd_core::input::{InputEvent, KeyCode, MouseButton, MousePosition};
use std::cell::RefCell;
use std::collections::VecDeque;

/// Предел длины очереди захваченных событий.
///
/// Очередь наполняется системой и опустошается циклом клиента. Если
/// цикл встал (например, идёт пересоздание стека кодеков), события
/// продолжают приходить — и без предела съели бы память.
///
/// 256 событий — это несколько секунд активной работы мышью. Переполнение
/// означает, что клиент не успевает, и старые события уже неактуальны:
/// поэтому вытесняется голова очереди, а не отбрасывается хвост.
const MAX_QUEUED: usize = 256;

thread_local! {
    /// Очередь захваченных событий.
    ///
    /// Thread-local, потому что оконная процедура вызывается в том же
    /// потоке, где крутится цикл сообщений. Мьютекс здесь был бы лишним
    /// и добавил бы блокировку в путь ввода.
    static EVENTS: RefCell<VecDeque<InputEvent>> = const { RefCell::new(VecDeque::new()) };

    /// Сколько событий вытеснено переполнением.
    static DROPPED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Положить событие в очередь захвата.
///
/// Вызывается из оконной процедуры. Публична, потому что окно живёт
/// в `bd-render`, а очередь — здесь: разделение соответствует границам
/// крейтов (CLAUDE.md §4.2), окно не должно знать про формат событий.
pub fn push_event(event: InputEvent) {
    EVENTS.with(|q| {
        let mut q = q.borrow_mut();
        if q.len() >= MAX_QUEUED {
            // Вытесняется самое старое: при переполнении свежие
            // события ценнее — устаревшее движение мыши не нужно
            // никому (та же логика, что у видеокадров, §5.3).
            q.pop_front();
            DROPPED.with(|d| d.set(d.get() + 1));
        }
        q.push_back(event);
    });
}

/// Забрать все накопленные события.
///
/// Очередь опустошается: событие выдаётся ровно один раз.
pub fn drain_events(out: &mut Vec<InputEvent>) {
    EVENTS.with(|q| {
        let mut q = q.borrow_mut();
        out.extend(q.drain(..));
    });
}

/// Сколько событий вытеснено из-за переполнения очереди.
///
/// Ненулевое значение означает, что цикл клиента не успевает за
/// вводом. Это диагностика, а не ошибка: показывать её надо, иначе
/// потеря ввода выглядит как «мышь дёргается» без объяснений.
pub fn dropped_count() -> u64 {
    DROPPED.with(|d| d.get())
}

/// Очистить очередь, не обрабатывая события.
///
/// Нужно при потере фокуса: события, накопленные до неё, уже
/// неактуальны, а отпускание клавиш формирует [`crate::InputTracker`].
pub fn clear_events() {
    EVENTS.with(|q| q.borrow_mut().clear());
}

/// Перевести виртуальный код Windows в HID-код.
///
/// # Почему из виртуального, а не из скан-кода
///
/// Окно получает и то, и другое: `wParam` несёт виртуальный код,
/// `lParam` — скан-код. Скан-код был бы прямее (он и есть физическая
/// клавиша), но в `WM_KEYDOWN` он приходит без префикса `0xE0`,
/// а признак расширенности лежит отдельным битом `lParam`. Виртуальные
/// коды уже различают левый и правый модификаторы, поэтому перевод
/// из них короче и не требует разбора битовых полей сообщения.
///
/// Раскладка здесь роли не играет: перевод идёт в **физический**
/// HID-код, и обратно в скан-код его переведёт хост
/// ([`super::keymap::hid_to_scancode`]).
pub fn virtual_key_to_hid(vk: u16, extended: bool) -> Option<KeyCode> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    let hid = match VIRTUAL_KEY(vk) {
        // Буквы: виртуальные коды совпадают с ASCII 'A'–'Z',
        // HID идёт подряд с 0x04 в том же алфавитном порядке.
        VIRTUAL_KEY(v @ 0x41..=0x5A) => 0x04 + (v - 0x41),
        // Цифры основного ряда: VK '1'–'9' → HID 0x1E.., ноль отдельно.
        VIRTUAL_KEY(v @ 0x31..=0x39) => 0x1E + (v - 0x31),
        VIRTUAL_KEY(0x30) => 0x27, // 0

        VK_RETURN => {
            // Enter на нумпаде — та же клавиша с флагом расширения.
            if extended {
                0x58
            } else {
                0x28
            }
        }
        VK_ESCAPE => 0x29,
        VK_BACK => 0x2A,
        VK_TAB => 0x2B,
        VK_SPACE => 0x2C,
        VK_OEM_MINUS => 0x2D,
        VK_OEM_PLUS => 0x2E,
        VK_OEM_4 => 0x2F,
        VK_OEM_6 => 0x30,
        VK_OEM_5 => 0x31,
        VK_OEM_1 => 0x33,
        VK_OEM_7 => 0x34,
        VK_OEM_3 => 0x35,
        VK_OEM_COMMA => 0x36,
        VK_OEM_PERIOD => 0x37,
        VK_OEM_2 => 0x38,
        VK_CAPITAL => 0x39,

        // Функциональные F1–F12 идут подряд в обеих системах.
        VIRTUAL_KEY(v @ 0x70..=0x7B) => 0x3A + (v - 0x70),

        VK_SNAPSHOT => 0x46,
        VK_SCROLL => 0x47,
        VK_PAUSE => 0x48,
        VK_INSERT => 0x49,
        VK_HOME => 0x4A,
        VK_PRIOR => 0x4B,
        VK_DELETE => 0x4C,
        VK_END => 0x4D,
        VK_NEXT => 0x4E,
        VK_RIGHT => 0x4F,
        VK_LEFT => 0x50,
        VK_DOWN => 0x51,
        VK_UP => 0x52,

        VK_NUMLOCK => 0x53,
        VK_DIVIDE => 0x54,
        VK_MULTIPLY => 0x55,
        VK_SUBTRACT => 0x56,
        VK_ADD => 0x57,
        VK_NUMPAD1 => 0x59,
        VK_NUMPAD2 => 0x5A,
        VK_NUMPAD3 => 0x5B,
        VK_NUMPAD4 => 0x5C,
        VK_NUMPAD5 => 0x5D,
        VK_NUMPAD6 => 0x5E,
        VK_NUMPAD7 => 0x5F,
        VK_NUMPAD8 => 0x60,
        VK_NUMPAD9 => 0x61,
        VK_NUMPAD0 => 0x62,
        VK_DECIMAL => 0x63,

        // Модификаторы. Windows различает стороны отдельными кодами;
        // общие VK_SHIFT/VK_CONTROL/VK_MENU приходят только из
        // GetKeyState, а не из WM_KEYDOWN, поэтому здесь их нет.
        VK_LCONTROL => 0xE0,
        VK_LSHIFT => 0xE1,
        VK_LMENU => 0xE2,
        VK_LWIN => 0xE3,
        VK_RCONTROL => 0xE4,
        VK_RSHIFT => 0xE5,
        VK_RMENU => 0xE6,
        VK_RWIN => 0xE7,

        // Общие коды всё же приходят, когда клавиша не различается
        // (например, при SendInput без флага стороны). Считаем левыми:
        // это чаще верно и всегда безопасно — залипания не будет,
        // потому что отпускание придёт с тем же кодом.
        VK_CONTROL => 0xE0,
        VK_SHIFT => 0xE1,
        VK_MENU => 0xE2,

        _ => return None,
    };

    Some(KeyCode(hid))
}

/// Собрать событие движения мыши из координат окна.
///
/// `lparam` сообщений мыши несёт координаты клиентской области
/// в младшем и старшем словах. Координаты **знаковые**: курсор,
/// уехавший левее или выше окна, даёт отрицательные значения, и
/// беззнаковое чтение превратило бы их в огромные числа.
pub fn mouse_position_from_lparam(lparam: isize, width: u32, height: u32) -> MousePosition {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    MousePosition::from_pixels(x, y, width, height)
}

/// Событие кнопки мыши по коду сообщения.
///
/// Возвращает `None` для сообщений, не относящихся к трём основным
/// кнопкам (боковые X1/X2 пока не поддерживаются, §7.2).
pub fn mouse_button_from_message(msg: u32) -> Option<(MouseButton, bool)> {
    use windows::Win32::UI::WindowsAndMessaging::*;

    match msg {
        WM_LBUTTONDOWN => Some((MouseButton::Left, true)),
        WM_LBUTTONUP => Some((MouseButton::Left, false)),
        WM_RBUTTONDOWN => Some((MouseButton::Right, true)),
        WM_RBUTTONUP => Some((MouseButton::Right, false)),
        WM_MBUTTONDOWN => Some((MouseButton::Middle, true)),
        WM_MBUTTONUP => Some((MouseButton::Middle, false)),
        _ => None,
    }
}

/// Величина прокрутки из `wparam` сообщения колеса.
///
/// Старшее слово `wparam` — знаковое число, кратное 120
/// (`WHEEL_DELTA`). Делением получаем щелчки; тачпады присылают
/// дробные доли, и они сохраняются.
pub fn wheel_delta_from_wparam(wparam: usize) -> f32 {
    let raw = ((wparam >> 16) & 0xFFFF) as u16 as i16;
    f32::from(raw) / 120.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        clear_events();
        DROPPED.with(|d| d.set(0));
    }

    #[test]
    fn events_are_queued_and_drained_once() {
        reset();
        push_event(InputEvent::ReleaseAll);
        push_event(InputEvent::Key {
            key: KeyCode::A,
            pressed: true,
        });

        let mut out = Vec::new();
        drain_events(&mut out);
        assert_eq!(out.len(), 2);

        // Второй вызов не должен вернуть те же события снова.
        out.clear();
        drain_events(&mut out);
        assert!(out.is_empty(), "события выданы повторно");
    }

    #[test]
    fn overflow_drops_oldest_not_newest() {
        // Клиент мог встать (пересоздание кодеков). Свежие события
        // ценнее устаревших — как и с видеокадрами.
        reset();
        for i in 0..MAX_QUEUED + 10 {
            push_event(InputEvent::Key {
                key: KeyCode(i as u16),
                pressed: true,
            });
        }

        let mut out = Vec::new();
        drain_events(&mut out);
        assert_eq!(out.len(), MAX_QUEUED, "очередь превысила предел");
        assert_eq!(dropped_count(), 10, "счётчик вытеснений неверен");

        // Последнее событие обязано уцелеть — оно самое свежее.
        let InputEvent::Key { key, .. } = out.last().copied().unwrap() else {
            panic!("не то событие");
        };
        assert_eq!(key, KeyCode((MAX_QUEUED + 9) as u16));
    }

    #[test]
    fn letters_and_digits_map_to_hid() {
        assert_eq!(virtual_key_to_hid(0x41, false), Some(KeyCode::A));
        assert_eq!(virtual_key_to_hid(0x5A, false), Some(KeyCode::Z));
        assert_eq!(virtual_key_to_hid(0x31, false), Some(KeyCode::DIGIT_1));
        // Ноль стоит вне диапазона 1–9 в обеих системах — типичное
        // место ошибки на единицу.
        assert_eq!(virtual_key_to_hid(0x30, false), Some(KeyCode(0x27)));
    }

    #[test]
    fn left_and_right_modifiers_are_distinguished() {
        // Если бы стороны схлопывались, отпускание правого Shift
        // не сняло бы левый — то самое залипание.
        assert_eq!(virtual_key_to_hid(0xA0, false), Some(KeyCode::LEFT_SHIFT));
        assert_eq!(virtual_key_to_hid(0xA1, false), Some(KeyCode::RIGHT_SHIFT));
        assert_ne!(
            virtual_key_to_hid(0xA2, false),
            virtual_key_to_hid(0xA3, false),
            "левый и правый Ctrl должны различаться"
        );
    }

    #[test]
    fn numpad_enter_differs_from_main_enter() {
        // Обе клавиши шлют VK_RETURN; различает их только флаг
        // расширения. Спутать — значит послать не ту клавишу.
        let main = virtual_key_to_hid(0x0D, false).unwrap();
        let numpad = virtual_key_to_hid(0x0D, true).unwrap();
        assert_ne!(main, numpad);
        assert_eq!(main, KeyCode::ENTER);
    }

    #[test]
    fn unknown_virtual_key_is_rejected() {
        assert!(virtual_key_to_hid(0x00, false).is_none());
        assert!(virtual_key_to_hid(0xFF, false).is_none());
    }

    #[test]
    fn negative_mouse_coordinates_do_not_wrap() {
        // Курсор левее окна даёт отрицательный x. Беззнаковое чтение
        // превратило бы -1 в 65535 и увело бы курсор в другой угол.
        let lparam =
            ((-5i16 as u16 as isize) & 0xFFFF) | (((-3i16 as u16 as isize) & 0xFFFF) << 16);
        let p = mouse_position_from_lparam(lparam, 1920, 1080);
        assert_eq!(p.x(), 0.0, "отрицательный x должен зажаться в 0");
        assert_eq!(p.y(), 0.0);
    }

    #[test]
    fn mouse_coordinates_map_to_fractions() {
        let lparam = 960isize | (540isize << 16);
        let p = mouse_position_from_lparam(lparam, 1920, 1080);
        assert!((p.x() - 0.5).abs() < 1e-6);
        assert!((p.y() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn wheel_delta_keeps_sign() {
        // Прокрутка вниз — отрицательная. Знак теряется, если читать
        // старшее слово как беззнаковое.
        assert_eq!(wheel_delta_from_wparam(120 << 16), 1.0);
        assert_eq!(
            wheel_delta_from_wparam((-120i16 as u16 as usize) << 16),
            -1.0
        );
    }

    #[test]
    fn button_messages_map_to_buttons() {
        use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_RBUTTONUP};
        assert_eq!(
            mouse_button_from_message(WM_LBUTTONDOWN),
            Some((MouseButton::Left, true))
        );
        assert_eq!(
            mouse_button_from_message(WM_RBUTTONUP),
            Some((MouseButton::Right, false))
        );
        // Боковые кнопки пока не поддерживаются (§7.2) — молча
        // игнорируются, а не превращаются в чужую кнопку.
        assert!(mouse_button_from_message(0x020B).is_none());
    }
}
