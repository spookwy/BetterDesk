//! Единый источник монотонного времени для всего проекта.
//!
//! # Зачем это отдельный модуль
//!
//! Главная метрика проекта — задержка glass-to-glass. Измерить её можно
//! только если каждый кадр несёт метку времени захвата, и эта метка
//! сравнима во всех компонентах пайплайна. Поэтому весь проект использует
//! [`now()`] и никогда не вызывает `std::time::Instant::now()` напрямую.
//!
//! Правило из CLAUDE.md §4.5. Реализовано на этапе 1, а не «потом»,
//! потому что дооснастить этим готовый пайплайн намного дороже.

use std::time::Duration;

/// Момент времени по монотонным часам процесса.
///
/// Обёртка над [`std::time::Instant`]. Монотонные часы не идут назад при
/// коррекции системного времени (NTP, смена часового пояса, ручной перевод),
/// поэтому разница двух таких меток всегда осмысленна.
///
/// Не сравним между процессами и машинами — для этого есть [`Timestamp`].
pub type Instant = std::time::Instant;

/// Текущий момент по монотонным часам.
///
/// Единственный разрешённый способ узнать время в горячем пути.
#[inline]
pub fn now() -> Instant {
    Instant::now()
}

/// Метка времени в микросекундах, пригодная для передачи по сети.
///
/// [`Instant`] нельзя сериализовать: его начало отсчёта своё в каждом
/// процессе. `Timestamp` — это микросекунды от [`Epoch`] конкретного
/// процесса, то есть число, которое можно положить в пакет.
///
/// # Сравнение меток между хостом и клиентом
///
/// Метки двух машин напрямую **несравнимы** — их эпохи не совпадают.
/// Чтобы измерить сетевую задержку, нужна оценка смещения часов
/// (обмен парами меток по управляющему каналу, как в NTP). Это задача
/// этапа 3; до тех пор задержка меряется на одной машине (localhost),
/// где эпоха общая.
///
/// Микросекунды выбраны сознательно: миллисекунд мало (весь наш бюджет —
/// 20–30 мс, разрешение в 1 мс съело бы 5% точности), наносекунды избыточны
/// и переполняют u32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Метка из микросекунд от эпохи процесса.
    #[inline]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Микросекунды от эпохи процесса.
    #[inline]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// Время, прошедшее с более ранней метки.
    ///
    /// Возвращает [`Duration::ZERO`], если `earlier` позже текущей метки:
    /// метки могут прийти из сети в произвольном порядке, и паниковать
    /// на данных от недоверенного пира недопустимо (CLAUDE.md §4.3.6).
    #[inline]
    pub fn saturating_since(self, earlier: Timestamp) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }
}

/// Начало отсчёта для [`Timestamp`] в пределах одного процесса.
///
/// Фиксируется при первом обращении и живёт до конца процесса.
#[derive(Debug, Clone, Copy)]
pub struct Epoch {
    start: Instant,
}

impl Epoch {
    /// Новая эпоха с началом отсчёта «сейчас».
    #[inline]
    pub fn new() -> Self {
        Self { start: now() }
    }

    /// Метка для указанного момента.
    #[inline]
    pub fn stamp(&self, at: Instant) -> Timestamp {
        // saturating: `at` раньше начала эпохи быть не может при
        // корректном использовании, но паника здесь недопустима.
        let micros = at.saturating_duration_since(self.start).as_micros();
        Timestamp::from_micros(micros.min(u64::MAX as u128) as u64)
    }

    /// Метка для текущего момента.
    #[inline]
    pub fn stamp_now(&self) -> Timestamp {
        self.stamp(now())
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_monotonic() {
        let epoch = Epoch::new();
        let a = epoch.stamp_now();
        let b = epoch.stamp_now();
        assert!(b >= a, "метки должны быть неубывающими");
    }

    #[test]
    fn saturating_since_does_not_panic_on_reordering() {
        // Пакеты из сети приходят в произвольном порядке. Вычитание
        // большей метки из меньшей обязано дать ноль, а не панику.
        let early = Timestamp::from_micros(100);
        let late = Timestamp::from_micros(500);
        assert_eq!(late.saturating_since(early), Duration::from_micros(400));
        assert_eq!(early.saturating_since(late), Duration::ZERO);
    }

    #[test]
    fn epoch_starts_near_zero() {
        let epoch = Epoch::new();
        let t = epoch.stamp_now();
        // Первая метка должна быть в пределах миллисекунды от нуля.
        assert!(t.as_micros() < 1_000, "получено {} мкс", t.as_micros());
    }
}
