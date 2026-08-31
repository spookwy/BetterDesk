//! Инструменты замера задержки.
//!
//! Единственный крейт, где разрешён `println!` (CLAUDE.md §4.4).

#![forbid(unsafe_code)]

pub mod any_encoder;
pub mod any_transport;
pub mod input_bridge;
