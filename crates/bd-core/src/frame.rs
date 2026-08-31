//! Описание кадра, общее для всего пайплайна.
//!
//! # Почему здесь нет пикселей
//!
//! Главное правило пайплайна (CLAUDE.md §4.2.3): кадр **не копируется
//! в системную память**. Копия GPU→CPU→GPU стоит 5–10 мс и одна съедает
//! треть бюджета задержки.
//!
//! Поэтому `bd-core` описывает только *метаданные* кадра — размер, формат,
//! тайминги. Сама поверхность (D3D11-текстура на Windows) живёт в
//! платформенном крейте и наружу отдаётся как непрозрачный хендл.
//! Так `bd-core` остаётся портируемым (§4.2.1), а пиксели не покидают GPU.

use crate::metrics::FrameTimings;

/// Формат пикселей поверхности.
///
/// Перечислены только те форматы, что реально встречаются в пайплайне:
/// захват отдаёт BGRA, аппаратный энкодер и декодер работают с NV12.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8 бит на канал, порядок B-G-R-A. Выход DXGI Desktop Duplication.
    Bgra8,
    /// Планарный YUV 4:2:0. Вход аппаратного энкодера, выход декодера.
    Nv12,
}

impl PixelFormat {
    /// Размер кадра в байтах при плотной упаковке.
    ///
    /// Нужен для оценки объёма и для софтверных путей. К GPU-текстурам
    /// напрямую не применим: у них своя выравненная раскладка (pitch).
    pub const fn packed_size(self, width: u32, height: u32) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            PixelFormat::Bgra8 => w * h * 4,
            // Y-плоскость целиком + UV в половинном разрешении.
            PixelFormat::Nv12 => w * h + w * h / 2,
        }
    }
}

/// Размеры кадра в пикселях.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameSize {
    /// Ширина в пикселях.
    pub width: u32,
    /// Высота в пикселях.
    pub height: u32,
}

impl FrameSize {
    /// Новый размер.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Число пикселей.
    pub const fn pixel_count(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Вырожденный ли размер.
    ///
    /// При смене разрешения и сворачивании окон система может отдать
    /// нулевой размер — это не повод паниковать, но и обрабатывать
    /// такой кадр бессмысленно.
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Метаданные захваченного кадра.
///
/// Сама поверхность здесь не хранится: она платформенная и остаётся в GPU
/// (см. заметку модуля). Эта структура едет рядом с ней через весь пайплайн
/// и несёт тайминги, по которым считается задержка.
#[derive(Debug, Clone, Copy)]
pub struct FrameInfo {
    /// Размеры.
    pub size: FrameSize,
    /// Формат пикселей поверхности.
    pub format: PixelFormat,
    /// Отметки времени на стадиях пайплайна.
    pub timings: FrameTimings,
    /// Изменилось ли содержимое экрана с прошлого кадра.
    ///
    /// DXGI отдаёт кадр и тогда, когда сменилась только позиция курсора.
    /// Такой кадр не нужно кодировать заново — экономия битрейта
    /// и такта энкодера (CLAUDE.md §5.1).
    pub content_changed: bool,
}

impl FrameInfo {
    /// Метаданные кадра.
    pub fn new(size: FrameSize, format: PixelFormat, timings: FrameTimings) -> Self {
        Self {
            size,
            format,
            timings,
            content_changed: true,
        }
    }

    /// Тот же кадр, но помеченный как не изменившийся по содержимому.
    pub fn unchanged(mut self) -> Self {
        self.content_changed = false;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_is_half_the_size_of_bgra() {
        let (w, h) = (1920, 1080);
        let bgra = PixelFormat::Bgra8.packed_size(w, h);
        let nv12 = PixelFormat::Nv12.packed_size(w, h);
        assert_eq!(bgra, 1920 * 1080 * 4);
        assert_eq!(nv12, 1920 * 1080 * 3 / 2);
        // NV12 ровно в 2.67 раза компактнее — ради этого энкодер его и хочет.
        assert!(nv12 * 2 < bgra);
    }

    #[test]
    fn empty_size_is_detected() {
        assert!(FrameSize::new(0, 1080).is_empty());
        assert!(FrameSize::new(1920, 0).is_empty());
        assert!(!FrameSize::new(1920, 1080).is_empty());
    }

    #[test]
    fn pixel_count_does_not_overflow_on_large_sizes() {
        // 8K не должен переполнить счётчик.
        let size = FrameSize::new(7680, 4320);
        assert_eq!(size.pixel_count(), 33_177_600);
    }
}
