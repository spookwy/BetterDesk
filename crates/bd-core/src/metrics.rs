//! Тайминги кадра и агрегированная статистика пайплайна.
//!
//! # Зачем
//!
//! Задержка — главный критерий проекта (docs/latency-budget.md). Знать
//! только итоговую цифру бесполезно: когда она вырастет, надо понимать,
//! какая стадия раздулась. Поэтому каждый кадр несёт отметки всех стадий,
//! а не только момент захвата.

use crate::time::Timestamp;
use std::time::Duration;

/// Стадия пайплайна, на которой отмечается время кадра.
///
/// Порядок вариантов совпадает с порядком прохождения кадра.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// Кадр получен от системы захвата (DXGI/WGC).
    Captured,
    /// Энкодер выдал сжатые данные.
    Encoded,
    /// Пакеты переданы в транспорт.
    Sent,
    /// Кадр собран из пакетов на приёмной стороне.
    Received,
    /// Декодер выдал поверхность.
    Decoded,
    /// Кадр показан (`Present`).
    Presented,
}

impl Stage {
    /// Все стадии по порядку прохождения.
    pub const ALL: [Stage; 6] = [
        Stage::Captured,
        Stage::Encoded,
        Stage::Sent,
        Stage::Received,
        Stage::Decoded,
        Stage::Presented,
    ];

    /// Короткое имя для оверлея статистики.
    pub const fn label(self) -> &'static str {
        match self {
            Stage::Captured => "capture",
            Stage::Encoded => "encode",
            Stage::Sent => "send",
            Stage::Received => "network",
            Stage::Decoded => "decode",
            Stage::Presented => "render",
        }
    }
}

/// Отметки времени одного кадра на всех стадиях пайплайна.
///
/// Структура едет вместе с кадром. Она намеренно `Copy` и без аллокаций:
/// создаётся 60 раз в секунду в горячем пути.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTimings {
    /// Порядковый номер кадра. Нужен для обнаружения потерь.
    pub sequence: u64,
    marks: [Option<Timestamp>; 6],
}

impl FrameTimings {
    /// Новые тайминги для кадра с заданным номером.
    pub fn new(sequence: u64) -> Self {
        Self {
            sequence,
            marks: [None; 6],
        }
    }

    /// Отметить прохождение стадии.
    ///
    /// Повторная отметка перезаписывает предыдущую: при повторной отправке
    /// кадра осмысленно последнее значение.
    #[inline]
    pub fn mark(&mut self, stage: Stage, at: Timestamp) {
        self.marks[stage as usize] = Some(at);
    }

    /// Отметка стадии, если она была пройдена.
    #[inline]
    pub fn get(&self, stage: Stage) -> Option<Timestamp> {
        self.marks[stage as usize]
    }

    /// Длительность одной стадии — от `from` до `to`.
    ///
    /// `None`, если хоть одна отметка отсутствует.
    pub fn span(&self, from: Stage, to: Stage) -> Option<Duration> {
        let a = self.get(from)?;
        let b = self.get(to)?;
        Some(b.saturating_since(a))
    }

    /// Полная задержка от захвата до показа.
    ///
    /// Это и есть измеряемая задержка пайплайна. Она **не равна**
    /// glass-to-glass: не учитывает ожидание кадра до захвата и задержку
    /// самого дисплея (docs/latency-budget.md §2). Годится для отслеживания
    /// регрессов, но не для маркетинговых заявлений.
    pub fn total(&self) -> Option<Duration> {
        self.span(Stage::Captured, Stage::Presented)
    }

    /// Длительности стадий: для каждой отмеченной — время от
    /// предыдущей отмеченной.
    ///
    /// Промежуточные стадии могут отсутствовать, и это норма: на
    /// этапе 1 транспорта ещё нет, поэтому `Sent` и `Received` не
    /// отмечены. Считать интервалы только между *соседними* стадиями
    /// значило бы в этом случае потерять и время декода — оно попало
    /// бы в пару `Received → Decoded`, которой не существует.
    ///
    /// Поэтому пары строятся по фактически отмеченным стадиям.
    /// Сумма выданных интервалов всегда равна [`total`](Self::total).
    pub fn breakdown(&self) -> impl Iterator<Item = (Stage, Duration)> + '_ {
        let mut previous: Option<Timestamp> = None;
        Stage::ALL.into_iter().filter_map(move |stage| {
            let at = self.get(stage)?;
            let from = previous.replace(at)?;
            Some((stage, at.saturating_since(from)))
        })
    }
}

/// Скользящая статистика задержки.
///
/// Хранит последние `capacity` значений и считает по ним медиану и
/// 95-й перцентиль. Перцентиль важнее среднего: редкие всплески
/// ощущаются пользователем сильнее, чем стабильное среднее
/// (docs/latency-budget.md §4).
#[derive(Debug, Clone)]
pub struct LatencyWindow {
    samples: Vec<Duration>,
    capacity: usize,
    next: usize,
}

impl LatencyWindow {
    /// Окно на `capacity` последних значений.
    ///
    /// # Паника
    ///
    /// Если `capacity` равна нулю. Это ошибка инициализации, а не
    /// обработка недоверенных данных, поэтому паника здесь допустима
    /// (CLAUDE.md §4.3.6).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ёмкость окна должна быть положительной");
        Self {
            samples: Vec::with_capacity(capacity),
            capacity,
            next: 0,
        }
    }

    /// Добавить измерение, вытеснив самое старое при переполнении.
    pub fn push(&mut self, value: Duration) {
        if self.samples.len() < self.capacity {
            self.samples.push(value);
        } else {
            self.samples[self.next] = value;
            self.next = (self.next + 1) % self.capacity;
        }
    }

    /// Число накопленных измерений.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Пусто ли окно.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Перцентиль от 0.0 до 1.0. `None` для пустого окна.
    ///
    /// Сортирует копию — вызывать для отображения (несколько раз в секунду),
    /// а не в горячем пути на каждый кадр.
    pub fn percentile(&self, p: f64) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let clamped = p.clamp(0.0, 1.0);
        let idx = ((sorted.len() - 1) as f64 * clamped).round() as usize;
        Some(sorted[idx])
    }

    /// Медиана.
    pub fn median(&self) -> Option<Duration> {
        self.percentile(0.5)
    }

    /// 95-й перцентиль — основной показатель для оценки качества.
    pub fn p95(&self) -> Option<Duration> {
        self.percentile(0.95)
    }

    /// Максимум за окно.
    pub fn max(&self) -> Option<Duration> {
        self.samples.iter().copied().max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(micros: u64) -> Timestamp {
        Timestamp::from_micros(micros)
    }

    #[test]
    fn total_spans_capture_to_present() {
        let mut t = FrameTimings::new(1);
        t.mark(Stage::Captured, ts(0));
        t.mark(Stage::Presented, ts(25_000));
        assert_eq!(t.total(), Some(Duration::from_millis(25)));
    }

    #[test]
    fn total_is_none_without_both_marks() {
        let mut t = FrameTimings::new(1);
        t.mark(Stage::Captured, ts(0));
        assert_eq!(t.total(), None, "без Presented итога быть не может");
    }

    #[test]
    fn breakdown_skips_missing_stages() {
        // Ситуация хоста: кадр захвачен, закодирован, отправлен — и всё.
        let mut t = FrameTimings::new(7);
        t.mark(Stage::Captured, ts(0));
        t.mark(Stage::Encoded, ts(4_000));
        t.mark(Stage::Sent, ts(5_000));

        let stages: Vec<_> = t.breakdown().collect();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0], (Stage::Encoded, Duration::from_millis(4)));
        assert_eq!(stages[1], (Stage::Sent, Duration::from_millis(1)));
    }

    #[test]
    fn breakdown_bridges_gap_in_the_middle() {
        // Ситуация этапа 1: транспорта ещё нет, поэтому Sent и
        // Received не отмечены. Время декода при этом теряться
        // не должно — иначе стадия молча пропадает из отчёта.
        let mut t = FrameTimings::new(8);
        t.mark(Stage::Captured, ts(0));
        t.mark(Stage::Encoded, ts(2_000));
        t.mark(Stage::Decoded, ts(2_300));
        t.mark(Stage::Presented, ts(2_400));

        let stages: Vec<_> = t.breakdown().collect();
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0], (Stage::Encoded, Duration::from_micros(2_000)));
        assert_eq!(
            stages[1],
            (Stage::Decoded, Duration::from_micros(300)),
            "декод обязан попасть в разбивку, даже без транспорта"
        );
        assert_eq!(stages[2], (Stage::Presented, Duration::from_micros(100)));
    }

    #[test]
    fn breakdown_sums_to_total() {
        // Разбивка обязана объяснять итог целиком: если сумма стадий
        // не равна total, значит какое-то время потерялось незаметно.
        let mut t = FrameTimings::new(9);
        t.mark(Stage::Captured, ts(0));
        t.mark(Stage::Encoded, ts(2_000));
        t.mark(Stage::Decoded, ts(2_300));
        t.mark(Stage::Presented, ts(2_400));

        let sum: Duration = t.breakdown().map(|(_, d)| d).sum();
        assert_eq!(Some(sum), t.total());
    }

    #[test]
    fn window_evicts_oldest_when_full() {
        let mut w = LatencyWindow::new(3);
        for ms in [10u64, 20, 30, 40] {
            w.push(Duration::from_millis(ms));
        }
        assert_eq!(w.len(), 3);
        // 10 вытеснено, остались 20/30/40.
        assert_eq!(w.max(), Some(Duration::from_millis(40)));
        assert_eq!(w.median(), Some(Duration::from_millis(30)));
    }

    #[test]
    fn percentiles_on_known_distribution() {
        // 101 значение (0..=100 мс) даёт целые индексы для медианы и p95,
        // поэтому тест не зависит от способа округления индекса.
        let mut w = LatencyWindow::new(101);
        for ms in 0..=100u64 {
            w.push(Duration::from_millis(ms));
        }
        assert_eq!(w.median(), Some(Duration::from_millis(50)));
        assert_eq!(w.p95(), Some(Duration::from_millis(95)));
        assert_eq!(w.max(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn percentile_bounds_are_min_and_max() {
        let mut w = LatencyWindow::new(10);
        for ms in [30u64, 10, 20] {
            w.push(Duration::from_millis(ms));
        }
        assert_eq!(w.percentile(0.0), Some(Duration::from_millis(10)));
        assert_eq!(w.percentile(1.0), Some(Duration::from_millis(30)));
        // Выход за границы диапазона зажимается, а не паникует.
        assert_eq!(w.percentile(-5.0), Some(Duration::from_millis(10)));
        assert_eq!(w.percentile(42.0), Some(Duration::from_millis(30)));
    }

    #[test]
    fn empty_window_reports_none() {
        let w = LatencyWindow::new(10);
        assert!(w.is_empty());
        assert_eq!(w.median(), None);
        assert_eq!(w.p95(), None);
    }
}
