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

    /// Настройки с явно заданным битрейтом.
    ///
    /// Всё остальное — как в [`low_latency`](Self::low_latency).
    /// Нужен там, где канал заведомо уже, чем позволяет разрешение:
    /// формула масштабирует битрейт от площади кадра, и монитор
    /// 2560x1600 просит 29.6 Мбит/с — больше, чем прокачает обычный
    /// интернет-канал.
    ///
    /// Границы те же, что у автоматического расчёта: ниже 1 Мбит/с
    /// картинка распадается независимо от настроек, выше 100 Мбит/с
    /// упирается в канал раньше, чем в энкодер.
    pub fn with_bitrate(size: FrameSize, fps: u32, bitrate: u32) -> Self {
        Self {
            rate_control: RateControl::Cbr {
                bitrate: bitrate.clamp(1_000_000, 100_000_000),
            },
            ..Self::low_latency(size, fps)
        }
    }

    /// Настройки по пресету качества.
    pub fn preset(size: FrameSize, fps: u32, preset: QualityPreset) -> Self {
        let auto = Self::low_latency(size, fps).rate_control.target_bitrate();
        Self::with_bitrate(size, fps, preset.apply(auto))
    }

    /// Ожидаемая длительность одного кадра.
    pub fn frame_duration(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps.max(1) as f64)
    }
}

/// Пресет качества — во сколько раз брать от автоматического битрейта.
///
/// Автоматический расчёт (§5.2) исходит из площади кадра и рассчитан
/// на канал, который его вытянет. Через интернет это неверно: канал
/// задан провайдером, а не разрешением монитора. Пресет — грубая
/// ручка на этот случай, до появления автоматического контроллера
/// битрейта на этапе 6.
///
/// Множители, а не абсолютные значения: 15 Мбит/с — это «много» для
/// 720p и «мало» для 4K, и одна и та же цифра означала бы разное
/// качество на разных машинах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityPreset {
    /// Экономный: четверть от расчётного. Для узкого или занятого
    /// канала — текст остаётся читаемым, градиенты грубеют.
    Low,
    /// Обычный: расчётный битрейт. То, что было до появления флага.
    Medium,
    /// Максимальный: полтора расчётных. Имеет смысл только там, где
    /// канал заведомо шире (локальная сеть).
    High,
}

impl QualityPreset {
    /// Применить пресет к автоматически рассчитанному битрейту.
    pub fn apply(self, auto_bitrate: u32) -> u32 {
        let scaled = match self {
            QualityPreset::Low => auto_bitrate as u64 / 4,
            QualityPreset::Medium => auto_bitrate as u64,
            QualityPreset::High => auto_bitrate as u64 * 3 / 2,
        };
        scaled.min(u32::MAX as u64) as u32
    }

    /// Разбор имени пресета из аргумента командной строки.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "low" => Some(QualityPreset::Low),
            "medium" => Some(QualityPreset::Medium),
            "high" => Some(QualityPreset::High),
            _ => None,
        }
    }

    /// Человекочитаемое имя для вывода.
    pub fn name(self) -> &'static str {
        match self {
            QualityPreset::Low => "низкое",
            QualityPreset::Medium => "среднее",
            QualityPreset::High => "высокое",
        }
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
    #[test]
    fn presets_are_ordered_and_medium_matches_auto() {
        let size = FrameSize::new(1920, 1080);
        let auto = EncoderConfig::low_latency(size, 60)
            .rate_control
            .target_bitrate();

        let low = EncoderConfig::preset(size, 60, QualityPreset::Low)
            .rate_control
            .target_bitrate();
        let medium = EncoderConfig::preset(size, 60, QualityPreset::Medium)
            .rate_control
            .target_bitrate();
        let high = EncoderConfig::preset(size, 60, QualityPreset::High)
            .rate_control
            .target_bitrate();

        assert!(low < medium, "низкое должно быть меньше среднего");
        assert!(medium < high, "среднее должно быть меньше высокого");
        assert_eq!(medium, auto, "medium обязан совпадать с расчётным");
    }

    #[test]
    fn explicit_bitrate_wins_over_scaling() {
        // Смысл флага: канал задан провайдером, а не разрешением.
        // Большой монитор не должен продавить заданное значение.
        let cfg = EncoderConfig::with_bitrate(FrameSize::new(2560, 1600), 60, 8_000_000);
        assert_eq!(cfg.rate_control.target_bitrate(), 8_000_000);
    }

    #[test]
    fn explicit_bitrate_is_clamped() {
        let size = FrameSize::new(1920, 1080);
        // Ноль и абсурдно большое значение не должны доходить до энкодера:
        // NVENC на нуле не откажет, а выдаст кашу вместо картинки.
        assert!(
            EncoderConfig::with_bitrate(size, 60, 0)
                .rate_control
                .target_bitrate()
                >= 1_000_000
        );
        assert!(
            EncoderConfig::with_bitrate(size, 60, u32::MAX)
                .rate_control
                .target_bitrate()
                <= 100_000_000
        );
    }

    #[test]
    fn preset_names_round_trip_and_reject_garbage() {
        for p in [
            QualityPreset::Low,
            QualityPreset::Medium,
            QualityPreset::High,
        ] {
            assert!(!p.name().is_empty());
        }
        assert_eq!(QualityPreset::parse("low"), Some(QualityPreset::Low));
        assert_eq!(QualityPreset::parse("HIGH"), Some(QualityPreset::High));
        // Разбор нечувствителен к регистру и пробелам: человек печатает
        // флаг руками, и `--quality LOW ` не должно быть ошибкой.
        assert_eq!(QualityPreset::parse(" Low "), Some(QualityPreset::Low));
        // Проверка обязана уметь отвергать — иначе она ничего не значит.
        assert_eq!(QualityPreset::parse("ultra"), None);
        assert_eq!(QualityPreset::parse(""), None);
    }

    #[test]
    fn preset_keeps_intra_refresh() {
        // Пресет меняет только битрейт. Intra Refresh обязателен по
        // §5.2 независимо от качества, и потерять его при смене
        // конструктора было бы тихим регрессом.
        let cfg = EncoderConfig::preset(FrameSize::new(1920, 1080), 60, QualityPreset::Low);
        assert!(cfg.intra_refresh);
        assert_eq!(cfg.fps, 60);
    }
}
