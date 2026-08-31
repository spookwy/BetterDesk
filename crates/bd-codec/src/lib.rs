//! Энкод и декод. `trait Encoder` / `trait Decoder` и бэкенды.
//!
//! Здесь живёт FFI к NVENC/AMF/QSV. Каждый ресурс C-API обёрнут в
//! RAII-тип с `Drop` (CLAUDE.md §4.3.4) — забыть освободить невозможно.
//!
//! # Абстракция кодека
//!
//! [`Encoder`] намеренно не привязан к H.264: [`Codec`] — параметр
//! настройки. Сейчас реализован только H.264 (§9.2), но переход на AV1
//! при появлении подходящего железа не должен требовать переделки
//! пайплайна. Это единственная страховка от патентных рисков H.264,
//! поэтому абстракцию не схлопывать «для простоты».

#![deny(unsafe_op_in_unsafe_fn)]

mod error;

pub use error::{CodecError, Result};

use bd_core::frame::FrameSize;
use std::time::Duration;

#[cfg(all(windows, nvenc_available))]
pub mod nvenc;

// Декод не привязан к NVENC: клиент может работать на машине без
// NVIDIA вовсе, а Media Foundation есть в любой Windows 8+.
#[cfg(windows)]
pub mod mediafoundation;

/// Видеокодек.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC. Аппаратная поддержка есть практически везде с 2012 года.
    H264,
    /// AV1. Беспатентный, но энкод требует RTX 40xx / RX 7000 / Arc.
    Av1,
}

/// Режим управления битрейтом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Постоянный битрейт. Выбор по умолчанию для нашей задачи:
    /// предсказуемая нагрузка на канал важнее пикового качества.
    Cbr {
        /// Целевой битрейт, бит/с.
        bitrate: u32,
    },
    /// Переменный битрейт с потолком.
    Vbr {
        /// Средний битрейт, бит/с.
        average: u32,
        /// Максимальный битрейт, бит/с.
        max: u32,
    },
}

impl RateControl {
    /// Целевой битрейт в битах в секунду.
    pub fn target_bitrate(self) -> u32 {
        match self {
            RateControl::Cbr { bitrate } => bitrate,
            RateControl::Vbr { average, .. } => average,
        }
    }
}

/// Настройки энкодера.
///
/// Значения по умолчанию соответствуют профилю низкой задержки из
/// CLAUDE.md §5.2: без B-кадров, без look-ahead, CBR, Intra Refresh
/// вместо периодических IDR.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Кодек.
    pub codec: Codec,
    /// Разрешение.
    pub size: FrameSize,
    /// Частота кадров, кадров в секунду.
    pub fps: u32,
    /// Управление битрейтом.
    pub rate_control: RateControl,
    /// Использовать Intra Refresh вместо полных ключевых кадров.
    ///
    /// Полный IDR даёт всплеск битрейта в несколько раз — на плохом
    /// канале это пик задержки. Intra Refresh размазывает обновление
    /// по нескольким кадрам (CLAUDE.md §5.2).
    pub intra_refresh: bool,
}

impl EncoderConfig {
    /// Настройки для потока с низкой задержкой.
    ///
    /// Битрейт по умолчанию — 15 Мбит/с для 1080p60: достаточно для
    /// чёткого текста, который и есть главный сценарий (CLAUDE.md §7.2).
    pub fn low_latency(size: FrameSize, fps: u32) -> Self {
        // Битрейт масштабируется от площади кадра: 1080p60 → ~15 Мбит/с.
        let pixels = size.pixel_count().max(1);
        let reference_pixels = 1920u64 * 1080;
        let scaled = 15_000_000u64 * pixels / reference_pixels * fps as u64 / 60;
        let bitrate = scaled.clamp(1_000_000, 100_000_000) as u32;

        Self {
            codec: Codec::H264,
            size,
            fps,
            rate_control: RateControl::Cbr { bitrate },
            intra_refresh: true,
        }
    }

    /// Ожидаемая длительность одного кадра.
    pub fn frame_duration(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps.max(1) as f64)
    }
}

/// Тип закодированного кадра.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Ключевой кадр — декодируется самостоятельно.
    Key,
    /// Разностный кадр — требует предыдущих.
    Delta,
}

/// Закодированный кадр.
#[derive(Debug)]
pub struct EncodedFrame {
    /// Сжатые данные (Annex B для H.264).
    pub data: Vec<u8>,
    /// Тип кадра.
    pub kind: FrameKind,
    /// Метаданные и тайминги исходного кадра.
    pub info: bd_core::frame::FrameInfo,
}

/// Аппаратный энкодер видео.
///
/// # Контракт по задержке
///
/// Реализация обязана принимать GPU-поверхность напрямую, без
/// копирования через системную память (CLAUDE.md §4.2.3).
pub trait Encoder: Send {
    /// Тип входного кадра, специфичный для бэкенда.
    type Input;

    /// Закодировать кадр.
    ///
    /// Возвращает `None`, если энкодер пока не отдал данные: при
    /// низкой задержке это редкость, но API допускает буферизацию.
    fn encode(&mut self, frame: &Self::Input) -> Result<Option<EncodedFrame>>;

    /// Запросить ключевой кадр (или начать Intra Refresh).
    ///
    /// Вызывается, когда клиент сообщил о потерях, которые не удалось
    /// восстановить.
    fn request_keyframe(&mut self);

    /// Сменить битрейт без пересоздания сессии.
    ///
    /// Нужно для адаптации к каналу (этап 6). Пересоздание энкодера
    /// на лету дало бы разрыв картинки, поэтому смена должна быть
    /// именно динамической.
    fn set_bitrate(&mut self, bitrate: u32) -> Result<()>;

    /// Текущие настройки.
    fn config(&self) -> &EncoderConfig;
}

/// Настройки декодера.
///
/// Разрешение здесь — подсказка для предварительного выделения
/// поверхностей, а не жёсткое требование: поток может сменить его на
/// лету (хост переключил монитор или сменилось разрешение экрана).
/// Реализация обязана это пережить, поэтому фактический размер кадра
/// берётся из самого выходного кадра, а не из этих настроек.
#[derive(Debug, Clone)]
pub struct DecoderConfig {
    /// Кодек потока.
    pub codec: Codec,
    /// Ожидаемое разрешение.
    pub size: FrameSize,
    /// Разрешить программный декод, если аппаратного нет.
    ///
    /// На хосте это не нужно (там всегда есть GPU с NVENC), а вот у
    /// клиента аппаратного декода может не быть вовсе — как на тестовой
    /// машине во Франции (CLAUDE.md §0.1). Тогда единственный путь —
    /// `openh264` (§5.5), и цена в миллисекундах принимается сознательно.
    pub allow_software: bool,
}

impl DecoderConfig {
    /// Настройки для потока с низкой задержкой.
    pub fn low_latency(size: FrameSize) -> Self {
        Self {
            codec: Codec::H264,
            size,
            allow_software: true,
        }
    }
}

/// Декодер сжатого видеопотока.
///
/// # Контракт по задержке
///
/// Аппаратная реализация обязана отдавать GPU-поверхность: копия
/// NV12-кадра 1080p в системную память и обратно стоит 5–10 мс
/// (CLAUDE.md §4.2.3). Программный фоллбэк это правило нарушает
/// вынужденно — он и так работает в CPU.
///
/// # Контракт по буферизации
///
/// Декодер **не должен** накапливать кадры. При настройках потока без
/// B-кадров (§5.2) на каждый вход приходится один выход, и любая
/// внутренняя очередь — это прямая добавка к задержке. Возврат `None`
/// допустим только пока декодер не получил достаточно данных для
/// первого кадра (SPS/PPS до первого IDR).
pub trait Decoder: Send {
    /// Тип выходного кадра, специфичный для бэкенда.
    ///
    /// У D3D11VA это NV12-текстура в GPU, у программного декодера —
    /// буфер в системной памяти. Верхние уровни видят его как
    /// непрозрачное значение и передают в рендер.
    type Output;

    /// Декодировать порцию сжатых данных.
    ///
    /// `data` — Annex B для H.264. Возвращает `None`, если кадр ещё
    /// не готов (см. контракт по буферизации выше).
    ///
    /// # Ошибки
    ///
    /// [`CodecError::CorruptStream`] означает потерю синхронизации:
    /// пропущен опорный кадр или данные повреждены. Это **не** повод
    /// пересоздавать декодер — надо запросить у хоста ключевой кадр
    /// (или Intra Refresh) и продолжать. На плохом канале это штатная
    /// ситуация, а не сбой.
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<Self::Output>>;

    /// Сбросить внутреннее состояние.
    ///
    /// Вызывается после разрыва потока, чтобы декодер не пытался
    /// опираться на кадры до разрыва.
    fn flush(&mut self) -> Result<()>;

    /// Текущие настройки.
    fn config(&self) -> &DecoderConfig;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bitrate_for_1080p60_is_reasonable() {
        let cfg = EncoderConfig::low_latency(FrameSize::new(1920, 1080), 60);
        assert_eq!(cfg.rate_control.target_bitrate(), 15_000_000);
        assert!(cfg.intra_refresh, "Intra Refresh обязателен для §5.2");
    }

    #[test]
    fn bitrate_scales_with_resolution() {
        let hd = EncoderConfig::low_latency(FrameSize::new(1280, 720), 60);
        let fhd = EncoderConfig::low_latency(FrameSize::new(1920, 1080), 60);
        assert!(
            hd.rate_control.target_bitrate() < fhd.rate_control.target_bitrate(),
            "720p должен требовать меньше, чем 1080p"
        );
    }

    #[test]
    fn bitrate_scales_with_fps() {
        let fps30 = EncoderConfig::low_latency(FrameSize::new(1920, 1080), 30);
        let fps60 = EncoderConfig::low_latency(FrameSize::new(1920, 1080), 60);
        assert!(fps30.rate_control.target_bitrate() < fps60.rate_control.target_bitrate());
    }

    #[test]
    fn bitrate_is_clamped_for_degenerate_sizes() {
        // Крошечный кадр не должен дать нулевой битрейт.
        let tiny = EncoderConfig::low_latency(FrameSize::new(16, 16), 60);
        assert!(tiny.rate_control.target_bitrate() >= 1_000_000);

        // 8K не должен уйти в абсурдные значения.
        let huge = EncoderConfig::low_latency(FrameSize::new(7680, 4320), 60);
        assert!(huge.rate_control.target_bitrate() <= 100_000_000);
    }

    #[test]
    fn frame_duration_matches_fps() {
        let cfg = EncoderConfig::low_latency(FrameSize::new(1920, 1080), 60);
        let ms = cfg.frame_duration().as_secs_f64() * 1000.0;
        assert!((ms - 16.667).abs() < 0.01, "получено {ms} мс");
    }
}
