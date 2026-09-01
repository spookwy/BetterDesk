//! Транспорт: QUIC, приоритеты, джиттер-буфер, congestion control.
//!
//! Не знает, что находится внутри пакетов (CLAUDE.md §4.2.4): двигает
//! байты с приоритетами и не парсит H.264. Это правило определяет всё
//! API крейта — наружу видны `&[u8]` и [`PayloadKind`], но не кадры и
//! не NAL-юниты.
//!
//! # Что здесь есть сейчас
//!
//! Этап 1 (docs/roadmap.md, пункт 3) — **транспорт-заглушка в памяти**,
//! [`LoopbackTransport`]. QUIC приходит на этапе 3.
//!
//! Настоящими здесь сделаны все части, кроме сети: wire-формат
//! ([`packet`]), фрагментация под MTU и сборка на приёме
//! ([`fragment`]), счётчики потерь. Причина — на этапе 3 замена
//! коснётся только доставки датаграмов, а логика, которую трудно
//! отлаживать поверх живой сети, уже будет проверена тестами.
//!
//! # Безопасность
//!
//! Разбор входящих датаграмов — первое, что встречает байты от
//! постороннего, ещё до всякой аутентификации (CLAUDE.md §8.5.3).
//! Поэтому: `#![forbid(unsafe_code)]`, никаких `unwrap` в пути разбора
//! (§4.3.6), все инварианты проверяются явно в
//! [`FragmentHeader::parse`], а сборщик кадров опирается только на уже
//! проверенные значения.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod discovery;
pub mod error;
pub mod fec;
pub mod fragment;
pub mod jitter;
pub mod loopback;
pub mod packet;
pub mod punch;
pub mod quic;

pub use discovery::{SignalEvent, Signaling};
pub use error::{Result, TransportError};
pub use fec::{FecPolicy, DEFAULT_REDUNDANCY_PERCENT};
pub use fragment::{
    DropReason, Fragmenter, ReassembledFrame, Reassembler, ReceiveOutcome, DEFAULT_MAX_PAYLOAD,
    MAX_FRAGMENTS,
};
pub use jitter::JitterBuffer;
pub use loopback::{LinkProfile, LoopbackTransport, TransportStats};
pub use packet::{FragmentHeader, PayloadKind, HEADER_SIZE, PROTOCOL_VERSION};
pub use punch::{punch, reachable_addr, PunchOutcome};
pub use quic::{QuicTransport, Role};
