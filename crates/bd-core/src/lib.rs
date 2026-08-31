//! Общее ядро BetterDesk: время, метрики, типы протокола.
//!
//! Этот крейт **не зависит ни от чего платформенного** и обязан
//! собираться под Linux и macOS (CLAUDE.md §4.2.1). Это дисциплинирует
//! API и оставляет открытой дверь для портирования.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clocksync;
pub mod cursor;
pub mod frame;
pub mod input;
pub mod metrics;
pub mod pacing;
pub mod time;

pub use clocksync::ClockSync;
pub use cursor::{CursorPosition, CursorShape, CursorShapeCache, CursorShapeKind};
pub use frame::{FrameInfo, FrameSize, PixelFormat};
pub use input::{InputEvent, KeyCode, MouseButton, MousePosition, SequencedInput};
pub use pacing::FrameLimiter;
pub use time::{now, Instant, Timestamp};
