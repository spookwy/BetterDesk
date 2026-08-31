//! Перевод HID-кодов клавиш в скан-коды PS/2 (набор 1).
//!
//! # Почему скан-коды, а не виртуальные коды
//!
//! `SendInput` умеет и то, и другое. Виртуальный код (`VK_A`) кажется
//! проще, но он **зависит от раскладки**: `VK_A` на клавиатуре AZERTY
//! соответствует физической клавише `Q`. Если передавать виртуальные
//! коды, то при разных раскладках у клиента и хоста человек нажимает
//! одну клавишу, а получает другую.
//!
//! Скан-код описывает **физическую клавишу** — то, что нажато на самом
//! деле. Раскладку применяет хост, как если бы человек сидел за ним.
//! Это и есть требуемое поведение: удалённая работа должна ощущаться
//! как локальная (docs/roadmap.md, этап 2).
//!
//! Отсюда флаг `KEYEVENTF_SCANCODE` при инжекте и таблица ниже.
//!
//! # Расширенные клавиши
//!
//! Часть клавиш (стрелки, правый Ctrl/Alt, Insert/Delete, Home/End,
//! NumLock) имеет скан-код, совпадающий с другой клавишей, и различается
//! префиксом `0xE0`. В `SendInput` этот префикс передаётся не в коде,
//! а флагом `KEYEVENTF_EXTENDEDKEY`. Забыть его — значит получить
//! стрелку вместо цифры на нумпаде: ошибка тихая и обнаруживается
//! только руками.

use bd_core::input::KeyCode;

/// Скан-код клавиши и признак расширенной.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    /// Код в наборе 1 (PS/2 Set 1), без префикса `0xE0`.
    pub code: u16,
    /// Нужен ли флаг `KEYEVENTF_EXTENDEDKEY`.
    pub extended: bool,
}

impl ScanCode {
    /// Обычная клавиша.
    const fn plain(code: u16) -> Self {
        Self {
            code,
            extended: false,
        }
    }

    /// Расширенная клавиша (префикс `0xE0`).
    const fn extended(code: u16) -> Self {
        Self {
            code,
            extended: true,
        }
    }
}

/// Перевести HID Usage (страница 0x07) в скан-код.
///
/// Возвращает `None` для кодов, которых нет в таблице. Это не ошибка:
/// HID определяет сотни клавиш, включая мультимедийные и специфичные
/// для отдельных клавиатур. Неизвестная клавиша просто не нажимается —
/// это лучше, чем нажать наугад не ту.
///
/// **Значения выверены по таблице USB HID Usage Tables 1.12, раздел 10
/// (Keyboard/Keypad Page).** Ошибка здесь не даёт ошибки компиляции —
/// только неверную букву при наборе, поэтому таблица покрыта тестами
/// на ключевых точках диапазонов.
pub const fn hid_to_scancode(key: KeyCode) -> Option<ScanCode> {
    let sc = match key.0 {
        // Буквы A–Z (HID 0x04–0x1D). Порядок HID алфавитный, а
        // скан-коды идут по раскладке QWERTY, поэтому таблица
        // перечисляется целиком — формулы здесь нет.
        0x04 => 0x1E, // A
        0x05 => 0x30, // B
        0x06 => 0x2E, // C
        0x07 => 0x20, // D
        0x08 => 0x12, // E
        0x09 => 0x21, // F
        0x0A => 0x22, // G
        0x0B => 0x23, // H
        0x0C => 0x17, // I
        0x0D => 0x24, // J
        0x0E => 0x25, // K
        0x0F => 0x26, // L
        0x10 => 0x32, // M
        0x11 => 0x31, // N
        0x12 => 0x18, // O
        0x13 => 0x19, // P
        0x14 => 0x10, // Q
        0x15 => 0x13, // R
        0x16 => 0x1F, // S
        0x17 => 0x14, // T
        0x18 => 0x16, // U
        0x19 => 0x2F, // V
        0x1A => 0x11, // W
        0x1B => 0x2D, // X
        0x1C => 0x15, // Y
        0x1D => 0x2C, // Z

        // Цифры 1–9, 0 (HID 0x1E–0x27). Здесь порядок совпадает,
        // и скан-коды идут подряд с 0x02.
        0x1E..=0x26 => key.0 - 0x1E + 0x02, // 1–9
        0x27 => 0x0B,                       // 0

        // Управляющие.
        0x28 => 0x1C, // Enter
        0x29 => 0x01, // Escape
        0x2A => 0x0E, // Backspace
        0x2B => 0x0F, // Tab
        0x2C => 0x39, // Space

        // Пунктуация основного блока.
        0x2D => 0x0C, // - _
        0x2E => 0x0D, // = +
        0x2F => 0x1A, // [ {
        0x30 => 0x1B, // ] }
        0x31 => 0x2B, // \ |
        0x33 => 0x27, // ; :
        0x34 => 0x28, // ' "
        0x35 => 0x29, // ` ~
        0x36 => 0x33, // , <
        0x37 => 0x34, // . >
        0x38 => 0x35, // / ?
        0x39 => 0x3A, // CapsLock

        // Функциональные F1–F10 (HID 0x3A–0x43) идут подряд с 0x3B.
        0x3A..=0x43 => key.0 - 0x3A + 0x3B,
        0x44 => 0x57, // F11
        0x45 => 0x58, // F12

        // Модификаторы левой стороны — обычные клавиши.
        0xE0 => 0x1D, // Left Ctrl
        0xE1 => 0x2A, // Left Shift
        0xE2 => 0x38, // Left Alt

        // Дальше — расширенные, им нужен флаг EXTENDEDKEY.
        0xE3 => return Some(ScanCode::extended(0x5B)), // Left Win
        0xE4 => return Some(ScanCode::extended(0x1D)), // Right Ctrl
        0xE5 => 0x36,                                  // Right Shift — НЕ расширенная
        0xE6 => return Some(ScanCode::extended(0x38)), // Right Alt (AltGr)
        0xE7 => return Some(ScanCode::extended(0x5C)), // Right Win

        0x46 => return Some(ScanCode::extended(0x37)), // PrintScreen
        0x47 => 0x46,                                  // ScrollLock
        0x48 => 0x45,                                  // Pause
        0x49 => return Some(ScanCode::extended(0x52)), // Insert
        0x4A => return Some(ScanCode::extended(0x47)), // Home
        0x4B => return Some(ScanCode::extended(0x49)), // PageUp
        0x4C => return Some(ScanCode::extended(0x53)), // Delete
        0x4D => return Some(ScanCode::extended(0x4F)), // End
        0x4E => return Some(ScanCode::extended(0x51)), // PageDown
        0x4F => return Some(ScanCode::extended(0x4D)), // Right Arrow
        0x50 => return Some(ScanCode::extended(0x4B)), // Left Arrow
        0x51 => return Some(ScanCode::extended(0x50)), // Down Arrow
        0x52 => return Some(ScanCode::extended(0x48)), // Up Arrow

        // Цифровой блок.
        0x53 => return Some(ScanCode::extended(0x45)), // NumLock
        0x54 => return Some(ScanCode::extended(0x35)), // Numpad /
        0x55 => 0x37,                                  // Numpad *
        0x56 => 0x4A,                                  // Numpad -
        0x57 => 0x4E,                                  // Numpad +
        0x58 => return Some(ScanCode::extended(0x1C)), // Numpad Enter
        0x59 => 0x4F,                                  // Numpad 1
        0x5A => 0x50,                                  // Numpad 2
        0x5B => 0x51,                                  // Numpad 3
        0x5C => 0x4B,                                  // Numpad 4
        0x5D => 0x4C,                                  // Numpad 5
        0x5E => 0x4D,                                  // Numpad 6
        0x5F => 0x47,                                  // Numpad 7
        0x60 => 0x48,                                  // Numpad 8
        0x61 => 0x49,                                  // Numpad 9
        0x62 => 0x52,                                  // Numpad 0
        0x63 => 0x53,                                  // Numpad .

        // Клавиша между левым Shift и Z на клавиатурах ISO.
        0x64 => 0x56,

        _ => return None,
    };
    Some(ScanCode::plain(sc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_qwerty_positions() {
        // Проверяются крайние точки и те буквы, где алфавитный порядок
        // HID расходится с раскладкой сильнее всего. Ошибка в таблице
        // не даёт ошибки компиляции — только неверную букву при наборе.
        assert_eq!(hid_to_scancode(KeyCode::A).unwrap().code, 0x1E);
        assert_eq!(hid_to_scancode(KeyCode::Z).unwrap().code, 0x2C);
        assert_eq!(hid_to_scancode(KeyCode(0x14)).unwrap().code, 0x10); // Q
        assert_eq!(hid_to_scancode(KeyCode(0x1A)).unwrap().code, 0x11); // W
    }

    #[test]
    fn digits_are_contiguous() {
        // Диапазон задан формулой, поэтому проверяются оба конца:
        // ошибка в смещении сдвинула бы все цифры разом.
        assert_eq!(hid_to_scancode(KeyCode::DIGIT_1).unwrap().code, 0x02);
        assert_eq!(hid_to_scancode(KeyCode(0x26)).unwrap().code, 0x0A); // 9
        assert_eq!(hid_to_scancode(KeyCode(0x27)).unwrap().code, 0x0B); // 0
    }

    #[test]
    fn function_keys_are_contiguous() {
        assert_eq!(hid_to_scancode(KeyCode(0x3A)).unwrap().code, 0x3B); // F1
        assert_eq!(hid_to_scancode(KeyCode(0x43)).unwrap().code, 0x44); // F10
        assert_eq!(hid_to_scancode(KeyCode(0x44)).unwrap().code, 0x57); // F11
        assert_eq!(hid_to_scancode(KeyCode(0x45)).unwrap().code, 0x58); // F12
    }

    #[test]
    fn right_alt_is_extended_but_right_shift_is_not() {
        // Ровно та пара, на которой легко ошибиться: правый Alt (AltGr)
        // расширенный, а правый Shift — нет, хотя оба «правые».
        // Ошибка здесь ломает ввод символов через AltGr на европейских
        // раскладках, и заметно это не сразу.
        let ralt = hid_to_scancode(KeyCode::RIGHT_ALT).unwrap();
        assert!(ralt.extended, "правый Alt обязан быть расширенным");
        assert_eq!(ralt.code, 0x38);

        let rshift = hid_to_scancode(KeyCode::RIGHT_SHIFT).unwrap();
        assert!(!rshift.extended, "правый Shift НЕ расширенный");
        assert_eq!(rshift.code, 0x36);
    }

    #[test]
    fn left_and_right_ctrl_share_code_but_differ_in_flag() {
        // Различаются только флагом — если его потерять, правый Ctrl
        // станет левым. Работать будет, но неотличимо неправильно.
        let l = hid_to_scancode(KeyCode::LEFT_CTRL).unwrap();
        let r = hid_to_scancode(KeyCode::RIGHT_CTRL).unwrap();
        assert_eq!(l.code, r.code, "у Ctrl общий скан-код");
        assert!(!l.extended);
        assert!(r.extended);
    }

    #[test]
    fn arrows_are_extended() {
        // Стрелки делят скан-коды с нумпадом. Без флага стрелка
        // превратится в цифру — классическая тихая ошибка.
        for hid in [0x4F, 0x50, 0x51, 0x52] {
            let sc = hid_to_scancode(KeyCode(hid)).unwrap();
            assert!(sc.extended, "стрелка HID {hid:#x} должна быть расширенной");
        }
    }

    #[test]
    fn arrows_and_numpad_share_codes() {
        // Прямая проверка того, ради чего нужен флаг: коды совпадают.
        let up = hid_to_scancode(KeyCode(0x52)).unwrap();
        let numpad_8 = hid_to_scancode(KeyCode(0x60)).unwrap();
        assert_eq!(up.code, numpad_8.code);
        assert_ne!(
            up.extended, numpad_8.extended,
            "различать их может только флаг"
        );
    }

    #[test]
    fn unknown_keys_are_not_guessed() {
        // HID определяет сотни клавиш. Нажать наугад не ту хуже,
        // чем не нажать ничего.
        assert!(hid_to_scancode(KeyCode(0x00)).is_none());
        assert!(hid_to_scancode(KeyCode(0xFFFF)).is_none());
        assert!(hid_to_scancode(KeyCode(0x9000)).is_none());
    }

    #[test]
    fn no_duplicate_plain_scancodes_among_letters() {
        // Опечатка в таблице чаще всего даёт дубликат: две буквы с
        // одним скан-кодом. Компилятор такого не увидит.
        let mut seen = std::collections::HashSet::new();
        for hid in 0x04..=0x1Du16 {
            let sc = hid_to_scancode(KeyCode(hid)).expect("буква обязана быть в таблице");
            assert!(
                seen.insert(sc.code),
                "скан-код {:#x} повторяется (HID {hid:#x})",
                sc.code
            );
        }
    }

    #[test]
    fn every_mapped_key_has_nonzero_code() {
        // Нулевой скан-код означал бы «клавиша не нажата» — такая
        // запись в таблице тихо ничего не делает.
        for hid in 0u16..=0xFF {
            if let Some(sc) = hid_to_scancode(KeyCode(hid)) {
                assert_ne!(sc.code, 0, "HID {hid:#x} даёт нулевой скан-код");
            }
        }
    }
}
