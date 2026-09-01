//! E2E-слой: идентичность устройства, Noise IK, пиннинг ключей.
//!
//! Своя криптография здесь не пишется — только сборка проверенных
//! примитивов (docs/security-model.md §2.2).
//!
//! Реализация — этап 5 (docs/roadmap.md).

#![forbid(unsafe_code)]

pub mod identity;
pub mod keystore;
pub mod password;
pub mod pinning;

pub use identity::{random_challenge, DeviceIdentity, DevicePublicKey, IdentityError};
pub use keystore::{FileKeyStore, KeyStore, KeyStoreError};
pub use password::{PasswordError, SessionPassword};
pub use pinning::{PinVerdict, PinnedKeys, PinningError};
