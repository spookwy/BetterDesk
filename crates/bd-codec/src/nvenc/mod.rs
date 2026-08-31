//! NVENC: аппаратный энкодер NVIDIA.
//!
//! # Устройство модуля
//!
//! - `sys` — сырые биндинги bindgen, наружу не выходят;
//! - `api` — загрузка DLL драйвера и таблицы функций;
//! - `versions` — версии структур (макросы, которые bindgen не переносит);
//! - `guids` — GUID кодеков и пресетов (то же самое);
//! - `session` — RAII-обёртка сессии кодирования;
//! - `resource` — регистрация текстур (zero-copy) и буфер битстрима;
//! - `encoder` — реализация [`Encoder`](crate::Encoder).
//!
//! Правило слоёв (CLAUDE.md §4.3.3): сырые указатели не покидают модуль.
//! Наружу отдаётся только безопасный API.

mod api;
mod encoder;
mod guids;
mod resource;
mod session;
mod sys;
mod versions;

pub use api::NvencApi;
pub use encoder::{EncoderInput, NvencEncoder};
pub use session::NvencSession;
