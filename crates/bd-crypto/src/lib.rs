//! E2E-слой: идентичность устройства, Noise IK, пиннинг ключей.
//!
//! Своя криптография здесь не пишется — только сборка проверенных
//! примитивов (docs/security-model.md §2.2).
//!
//! Реализация — этап 5 (docs/roadmap.md).

#![forbid(unsafe_code)]

pub mod password;

pub use password::{PasswordError, SessionPassword};
