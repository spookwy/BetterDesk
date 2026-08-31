//! Windows-реализация ввода.
//!
//! Весь `unsafe` крейта живёт здесь. Наружу уходят только безопасные
//! типы (CLAUDE.md §4.3).

pub mod capture;
mod injector;
mod keymap;

pub use injector::InputInjector;
pub use keymap::{hid_to_scancode, ScanCode};
