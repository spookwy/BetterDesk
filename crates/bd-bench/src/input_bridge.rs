//! Перевод сырых сообщений окна в события ввода.
//!
//! # Почему это здесь, а не в крейте
//!
//! Мост видит **оба** крейта: `bd-render` (откуда приходят сообщения)
//! и `bd-input` (куда уходят события). Положить его в любой из них
//! значило бы создать зависимость между ними, а окно вывода и ввод —
//! разные подсистемы, которые связывает потребитель (CLAUDE.md §4.2.5).
//!
//! В продукте этот код переедет в `bd-client`, который так же видит
//! оба крейта. Пока клиента нет, он живёт в пробе.

// NVENC здесь ни при чём: модуль про транспорт и ввод, а не про
// кодирование. Флаг стоял тут с тех пор, когда NVENC был обязателен
// для сборки вообще, — и после находки 61 стал бы отключать модуль
// на машине без LLVM без всякой причины.
#![cfg(windows)]

use bd_core::input::{InputEvent, MousePosition};
use bd_input::windows::capture::{
    mouse_button_from_message, mouse_position_from_lparam, virtual_key_to_hid,
    wheel_delta_from_wparam,
};
use bd_render::windows::RawInputMessage;

/// Бит `lParam`, помечающий расширенную клавишу.
///
/// Различает правый Ctrl/Alt от левых и стрелки от нумпада. Без него
/// правый Alt поехал бы как левый, и AltGr перестал бы работать
/// на европейских раскладках.
const EXTENDED_KEY_BIT: isize = 1 << 24;

/// Перевести сырое сообщение в событие ввода.
///
/// `None` означает «сообщение не несёт события» — например, клавиша
/// без HID-кода или кнопка, которую мы не поддерживаем (§7.2).
/// Это штатная ситуация, а не ошибка.
pub fn translate(
    raw: RawInputMessage,
    window_width: u32,
    window_height: u32,
) -> Option<InputEvent> {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    match raw.message {
        WM_MOUSEMOVE => Some(InputEvent::MouseMove {
            position: mouse_position_from_lparam(raw.lparam, window_width, window_height),
        }),

        WM_MOUSEWHEEL => Some(InputEvent::MouseScroll {
            delta_y: wheel_delta_from_wparam(raw.wparam),
            delta_x: 0.0,
        }),

        WM_MOUSEHWHEEL => Some(InputEvent::MouseScroll {
            delta_y: 0.0,
            delta_x: wheel_delta_from_wparam(raw.wparam),
        }),

        // Потеря фокуса: клавиши, зажатые сейчас, отпустить уже не
        // выйдет — их отпускание получит другое окно. Разворачивает
        // это событие в конкретные отпускания `InputTracker`.
        WM_KILLFOCUS => Some(InputEvent::ReleaseAll),

        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let extended = raw.lparam & EXTENDED_KEY_BIT != 0;
            let key = virtual_key_to_hid(raw.wparam as u16, extended)?;
            Some(InputEvent::Key {
                key,
                // Системные сообщения приходят парами со своими
                // обычными: Alt даёт WM_SYSKEYDOWN, а буква с
                // зажатым Alt — тоже системное. Признак нажатия
                // определяется кодом сообщения, а не наличием «SYS».
                pressed: matches!(raw.message, WM_KEYDOWN | WM_SYSKEYDOWN),
            })
        }

        message => {
            // Кнопки мыши: код сообщения сам несёт и кнопку, и
            // состояние, поэтому отдельной ветки на каждую не нужно.
            let (button, pressed) = mouse_button_from_message(message)?;
            Some(InputEvent::MouseButton {
                button,
                pressed,
                // Позиция едет вместе с кликом: если предыдущее
                // движение потерялось, клик всё равно придётся туда,
                // куда целился человек.
                position: mouse_position_from_lparam(raw.lparam, window_width, window_height),
            })
        }
    }
}

/// Подставить позицию курсора в события отпускания кнопок.
///
/// [`bd_input::InputTracker::release_events`] не знает, где курсор, и
/// ставит `(0, 0)`. Применить это буквально значило бы дёрнуть курсор
/// в левый верхний угол вместе с отпусканием кнопки.
pub fn with_position(event: InputEvent, position: MousePosition) -> InputEvent {
    match event {
        InputEvent::MouseButton {
            button, pressed, ..
        } => InputEvent::MouseButton {
            button,
            pressed,
            position,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::input::{KeyCode, MouseButton};
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_SYSKEYDOWN,
    };

    fn raw(message: u32, wparam: usize, lparam: isize) -> RawInputMessage {
        RawInputMessage {
            message,
            wparam,
            lparam,
        }
    }

    #[test]
    fn key_press_and_release_differ() {
        let down = translate(raw(WM_KEYDOWN, 0x41, 0), 800, 600).unwrap();
        let up = translate(raw(WM_KEYUP, 0x41, 0), 800, 600).unwrap();

        assert_eq!(
            down,
            InputEvent::Key {
                key: KeyCode::A,
                pressed: true
            }
        );
        assert_eq!(
            up,
            InputEvent::Key {
                key: KeyCode::A,
                pressed: false
            }
        );
    }

    #[test]
    fn alt_arrives_as_system_key_and_counts_as_pressed() {
        // Alt приходит через WM_SYSKEYDOWN. Если не считать это
        // нажатием, Alt останется зажатым на хосте — залипание,
        // которое запрещает критерий этапа 2.
        let event = translate(raw(WM_SYSKEYDOWN, 0xA4, 0), 800, 600).unwrap();
        let InputEvent::Key { key, pressed } = event else {
            panic!("не клавиша: {event:?}");
        };
        assert_eq!(key, KeyCode::LEFT_ALT);
        assert!(pressed, "WM_SYSKEYDOWN — это нажатие");
    }

    #[test]
    fn extended_bit_distinguishes_right_alt() {
        // Тот же виртуальный код, разный бит lParam. Потерять его —
        // значит сломать AltGr на европейских раскладках.
        let left = translate(raw(WM_SYSKEYDOWN, 0xA4, 0), 800, 600).unwrap();
        let right = translate(raw(WM_SYSKEYDOWN, 0xA5, EXTENDED_KEY_BIT), 800, 600).unwrap();
        assert_ne!(left, right);
    }

    #[test]
    fn kill_focus_becomes_release_all() {
        // Ключевое звено защиты от залипания: окно потеряло фокус,
        // значит отпускания клавиш до нас уже не дойдут.
        let event = translate(raw(WM_KILLFOCUS, 0, 0), 800, 600).unwrap();
        assert_eq!(event, InputEvent::ReleaseAll);
    }

    #[test]
    fn mouse_move_normalizes_to_window_size() {
        // Координаты приходят в пикселях окна, а уезжают долями:
        // разрешение хоста может отличаться от размера окна.
        let lparam = 400isize | (300isize << 16);
        let event = translate(raw(WM_MOUSEMOVE, 0, lparam), 800, 600).unwrap();
        let InputEvent::MouseMove { position } = event else {
            panic!("не движение: {event:?}");
        };
        assert!((position.x() - 0.5).abs() < 1e-6);
        assert!((position.y() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn click_carries_its_position() {
        // Позиция едет с кликом, чтобы потеря предыдущего движения
        // не увела клик мимо цели.
        let lparam = 400isize | (300isize << 16);
        let event = translate(raw(WM_LBUTTONDOWN, 0, lparam), 800, 600).unwrap();
        let InputEvent::MouseButton {
            button,
            pressed,
            position,
        } = event
        else {
            panic!("не кнопка: {event:?}");
        };
        assert_eq!(button, MouseButton::Left);
        assert!(pressed);
        assert!((position.x() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn wheel_maps_to_vertical_scroll() {
        let event = translate(raw(WM_MOUSEWHEEL, 120 << 16, 0), 800, 600).unwrap();
        assert_eq!(
            event,
            InputEvent::MouseScroll {
                delta_y: 1.0,
                delta_x: 0.0
            }
        );
    }

    #[test]
    fn unknown_message_yields_nothing() {
        // Неизвестное сообщение не должно превращаться в чужое
        // событие: лучше не сделать ничего, чем сделать не то.
        assert!(translate(raw(0x9999, 0, 0), 800, 600).is_none());
    }

    #[test]
    fn unmapped_key_yields_nothing() {
        assert!(translate(raw(WM_KEYDOWN, 0x00, 0), 800, 600).is_none());
    }

    #[test]
    fn with_position_fills_button_release() {
        // Трекер отпускает кнопки с позицией (0,0). Применить это
        // буквально — значит дёрнуть курсор в угол экрана.
        let release = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: false,
            position: MousePosition::new(0.0, 0.0),
        };
        let fixed = with_position(release, MousePosition::new(0.5, 0.5));
        let InputEvent::MouseButton { position, .. } = fixed else {
            panic!("вид события изменился");
        };
        assert!((position.x() - 0.5).abs() < 1e-4, "позиция не подставлена");
    }

    #[test]
    fn with_position_leaves_other_events_alone() {
        let key = InputEvent::Key {
            key: KeyCode::A,
            pressed: false,
        };
        assert_eq!(with_position(key, MousePosition::new(0.5, 0.5)), key);
    }
}
