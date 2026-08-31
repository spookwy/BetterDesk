//! Аппаратный декод через Media Foundation (D3D11VA).
//!
//! # Устройство модуля
//!
//! - `runtime` — инициализация Media Foundation и менеджер устройств
//!   DXGI, оба RAII;
//! - `decoder` — реализация [`Decoder`](crate::Decoder) поверх H.264 MFT.
//!
//! Правило слоёв (CLAUDE.md §4.3.3): сырые указатели не покидают
//! модуль. Наружу отдаётся [`DecodedFrame`] с GPU-текстурой, время
//! жизни которой связано с временем жизни самого кадра.
//!
//! Софтверный фоллбэк на `openh264` (§5.5) появится отдельным модулем
//! на этапе 4 — он нужен машинам без аппаратного декода.

mod decoder;
mod encoder;
mod runtime;
mod survey;

pub use decoder::{D3d11Decoder, DecodedFrame};
pub use encoder::{MfEncoder, MfEncoderInput, RawTextureInput};
pub use survey::{h264_encoders, EncoderEntry};
