//! Джиттер-буфер: сглаживание неравномерного прихода кадров.
//!
//! # Что он лечит
//!
//! Кадры уходят от хоста равномерно — раз в 16.6 мс при 60 fps. Но
//! приходят они неравномерно: сеть задерживает пакеты по-разному,
//! и разброс (джиттер) на Wi-Fi легко достигает десятков миллисекунд.
//!
//! Показывать кадры сразу по приходе — значит воспроизвести этот
//! разброс на экране: картинка идёт рывками, хотя ни один кадр не
//! потерян. Буфер выравнивает поток, придерживая ранние кадры.
//!
//! # Чем он опасен
//!
//! **Буфер — это задержка в чистом виде.** Каждая миллисекунда
//! удержания напрямую добавляется к glass-to-glass, ради которого
//! весь проект и затеян (CLAUDE.md, цель 20–30 мс в LAN).
//!
//! Поэтому здесь нет фиксированного размера «на всякий случай»:
//! буфер держит ровно столько, сколько показал измеренный джиттер,
//! и сжимается, когда сеть успокаивается. На идеальном канале он
//! обязан быть равен нулю — иначе мы платим за услугу, которая
//! не нужна.
//!
//! Это прямое следствие §5.3: устаревший кадр не нужен. Кадр,
//! пролежавший в буфере лишнее, — тоже устаревший.
//!
//! # Почему не «N кадров»
//!
//! Классический буфер меряется в кадрах. Здесь он меряется во
//! **времени**, потому что кадры приходят не равномерно: при
//! переменном битрейте «два кадра» — это то 33 мс, то 5 мс. Время
//! же напрямую сопоставимо с бюджетом задержки.

use crate::fragment::ReassembledFrame;
use bd_core::time::Timestamp;
use std::collections::VecDeque;
use std::time::Duration;

/// Сколько наблюдений джиттера держать в окне.
///
/// При 60 fps это около двух секунд — достаточно, чтобы уловить
/// характер канала, и мало, чтобы буфер успевал сжаться, когда сеть
/// успокоилась. Длинное окно означало бы, что один всплеск полминуты
/// держит буфер раздутым.
const JITTER_WINDOW: usize = 120;

/// Верхний предел удержания.
///
/// Больше этого буфер не растёт, даже если джиттер огромен. Причина
/// не техническая, а продуктовая: при задержке в 150 мс работать
/// невозможно, и «плавная, но неотзывчивая» картинка хуже рваной.
/// Лучше показать рывки и дать человеку понять, что канал плох.
const MAX_HOLD: Duration = Duration::from_millis(150);

/// Сколько кадров держать в очереди максимум.
///
/// Защита от переполнения памяти, а не политика: при нормальной
/// работе очередь держит единицы кадров. Двадцать при 60 fps — это
/// треть секунды, заведомо больше [`MAX_HOLD`].
const MAX_QUEUED: usize = 20;

/// Кадр, ожидающий своего момента.
struct Buffered {
    frame: ReassembledFrame,
    /// Когда кадр можно показывать.
    due: Timestamp,
    /// Когда он пришёл — для диагностики фактического удержания.
    arrived: Timestamp,
}

/// Адаптивный джиттер-буфер.
///
/// Кадры кладутся по приходе и забираются, когда наступает их время.
/// Размер удержания подстраивается под измеренный разброс задержек.
pub struct JitterBuffer {
    queue: VecDeque<Buffered>,
    /// Наблюдения задержки доставки, микросекунды.
    delays: VecDeque<u64>,
    /// Текущее удержание.
    hold: Duration,
    /// Номер последнего выданного кадра.
    ///
    /// Кадры с меньшим номером опоздали безнадёжно и выбрасываются:
    /// показать их после более нового — значит дёрнуть картинку
    /// назад во времени.
    last_delivered: Option<u64>,
    /// Сколько кадров выброшено как опоздавшие.
    late_dropped: u64,
    /// Сколько кадров выброшено из-за переполнения очереди.
    overflow_dropped: u64,
    /// Суммарное фактическое удержание, для средней цифры в отчёте.
    total_held_micros: u64,
    /// Сколько кадров выдано.
    delivered: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    /// Пустой буфер с нулевым удержанием.
    ///
    /// Ноль — правильное начальное значение: пока о канале ничего не
    /// известно, добавлять задержку не за что.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            delays: VecDeque::with_capacity(JITTER_WINDOW),
            hold: Duration::ZERO,
            last_delivered: None,
            late_dropped: 0,
            overflow_dropped: 0,
            total_held_micros: 0,
            delivered: 0,
        }
    }

    /// Положить пришедший кадр.
    ///
    /// `delay_micros` — измеренная задержка доставки этого кадра
    /// (её даёт [`bd_core::ClockSync::observe`]). Именно из разброса
    /// этих значений и вычисляется удержание.
    ///
    /// Кадр с номером меньше уже выданного отбрасывается: он опоздал
    /// безнадёжно, а показать его после более свежего означало бы
    /// движение картинки назад.
    pub fn push(&mut self, frame: ReassembledFrame, delay_micros: u64, now: Timestamp) {
        if let Some(last) = self.last_delivered {
            if frame.sequence <= last {
                self.late_dropped += 1;
                return;
            }
        }

        self.observe_delay(delay_micros);

        // Переполнение означает, что потребитель не забирает кадры.
        // Вытесняется самый старый: свежие кадры ценнее (§5.3).
        if self.queue.len() >= MAX_QUEUED {
            self.queue.pop_front();
            self.overflow_dropped += 1;
        }

        let due =
            Timestamp::from_micros(now.as_micros().saturating_add(self.hold.as_micros() as u64));

        // Очередь упорядочивается по номеру кадра, а не по приходу:
        // пакеты обгоняют друг друга, и выдать их в порядке прибытия
        // значило бы отдать декодеру перемешанный поток.
        let position = self
            .queue
            .iter()
            .rposition(|b| b.frame.sequence < frame.sequence)
            .map_or(0, |i| i + 1);

        self.queue.insert(
            position,
            Buffered {
                frame,
                due,
                arrived: now,
            },
        );
    }

    /// Забрать кадр, если его время пришло.
    ///
    /// `None` означает «рано» или «пусто» — и то, и другое штатно.
    pub fn pop(&mut self, now: Timestamp) -> Option<ReassembledFrame> {
        let front = self.queue.front()?;
        if front.due.as_micros() > now.as_micros() {
            return None;
        }

        let buffered = self.queue.pop_front()?;
        self.last_delivered = Some(buffered.frame.sequence);
        self.delivered += 1;
        self.total_held_micros += now.as_micros().saturating_sub(buffered.arrived.as_micros());

        Some(buffered.frame)
    }

    /// Учесть задержку доставки и пересчитать удержание.
    fn observe_delay(&mut self, delay_micros: u64) {
        self.delays.push_back(delay_micros);
        if self.delays.len() > JITTER_WINDOW {
            self.delays.pop_front();
        }

        // Удержание = разброс задержек внутри окна.
        //
        // Берётся не среднее и не максимум, а расстояние от минимума
        // до 95-го перцентиля. Максимум испортил бы буфер одним
        // выбросом — на Wi-Fi такие бывают постоянно и ни о чём не
        // говорят. Среднее, наоборот, недооценивает разброс: сгладить
        // надо именно хвост, а не типичный случай.
        let mut sorted: Vec<u64> = self.delays.iter().copied().collect();
        sorted.sort_unstable();

        let min = sorted.first().copied().unwrap_or(0);
        let p95 = sorted
            .get(sorted.len() * 95 / 100)
            .or_else(|| sorted.last())
            .copied()
            .unwrap_or(0);

        let spread = Duration::from_micros(p95.saturating_sub(min));
        self.hold = spread.min(MAX_HOLD);
    }

    /// Текущее удержание.
    pub fn hold(&self) -> Duration {
        self.hold
    }

    /// Сколько кадров ждёт в очереди.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Сколько кадров выброшено как безнадёжно опоздавшие.
    pub fn late_dropped(&self) -> u64 {
        self.late_dropped
    }

    /// Сколько кадров выброшено из-за переполнения очереди.
    pub fn overflow_dropped(&self) -> u64 {
        self.overflow_dropped
    }

    /// Среднее фактическое удержание кадра.
    ///
    /// Отличается от [`Self::hold`]: то — целевое значение, это —
    /// сколько кадры пролежали на самом деле. Расхождение означает,
    /// что потребитель забирает кадры не вовремя, и это надо видеть.
    pub fn average_held(&self) -> Duration {
        if self.delivered == 0 {
            return Duration::ZERO;
        }
        Duration::from_micros(self.total_held_micros / self.delivered)
    }

    /// Выбросить всё содержимое.
    ///
    /// Нужно при смене разрешения: кадры прежней геометрии декодеру
    /// уже не подходят.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.last_delivered = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::PayloadKind;

    fn frame(sequence: u64) -> ReassembledFrame {
        ReassembledFrame {
            kind: PayloadKind::Video,
            sequence,
            keyframe: false,
            captured_at_micros: sequence * 16_000,
            data: vec![sequence as u8; 8],
        }
    }

    fn at(micros: u64) -> Timestamp {
        Timestamp::from_micros(micros)
    }

    #[test]
    fn perfect_channel_adds_no_delay() {
        // Главное свойство: на канале без джиттера буфер обязан быть
        // прозрачным. Любое удержание здесь — это задержка, за
        // которую мы ничего не получаем.
        let mut b = JitterBuffer::new();

        for i in 0..30u64 {
            let now = at(i * 16_000);
            // Задержка постоянная — джиттера нет.
            b.push(frame(i), 5_000, now);
            assert_eq!(b.hold(), Duration::ZERO, "буфер держит без причины");
            assert!(b.pop(now).is_some(), "кадр должен выдаваться сразу");
        }
    }

    #[test]
    fn jitter_grows_the_hold() {
        // Разброс задержек обязан превратиться в удержание — иначе
        // буфер не выполняет свою работу.
        let mut b = JitterBuffer::new();

        for i in 0..40u64 {
            // Задержки скачут от 2 до 20 мс.
            let delay = if i % 2 == 0 { 2_000 } else { 20_000 };
            b.push(frame(i), delay, at(i * 16_000));
        }

        assert!(
            b.hold() >= Duration::from_millis(15),
            "удержание {:?} не покрывает разброс 18 мс",
            b.hold()
        );
    }

    #[test]
    fn hold_never_exceeds_the_cap() {
        // При чудовищном джиттере буфер обязан сдаться, а не расти:
        // задержка в секунду хуже рваной картинки.
        let mut b = JitterBuffer::new();

        for i in 0..40u64 {
            let delay = if i % 2 == 0 { 0 } else { 5_000_000 };
            b.push(frame(i), delay, at(i * 16_000));
        }

        assert_eq!(b.hold(), MAX_HOLD, "предел удержания не соблюдён");
    }

    #[test]
    fn hold_shrinks_when_network_calms_down() {
        // Буфер обязан сжиматься: иначе один всплеск в начале сессии
        // навсегда добавил бы задержку.
        let mut b = JitterBuffer::new();

        // Сначала шторм.
        for i in 0..JITTER_WINDOW as u64 {
            let delay = if i % 2 == 0 { 1_000 } else { 40_000 };
            b.push(frame(i), delay, at(i * 16_000));
            let _ = b.pop(at(i * 16_000 + 100_000));
        }
        let stormy = b.hold();
        assert!(stormy > Duration::from_millis(30));

        // Затем штиль — ровно столько кадров, чтобы окно обновилось.
        for i in JITTER_WINDOW as u64..(JITTER_WINDOW as u64 * 2) {
            b.push(frame(i), 3_000, at(i * 16_000));
            let _ = b.pop(at(i * 16_000 + 100_000));
        }

        assert!(
            b.hold() < stormy,
            "буфер не сжался: было {stormy:?}, стало {:?}",
            b.hold()
        );
    }

    #[test]
    fn frames_come_out_in_order_despite_reordering() {
        // Пакеты обгоняют друг друга. Выдать их в порядке прибытия —
        // значит отдать декодеру перемешанный поток H.264.
        let mut b = JitterBuffer::new();

        b.push(frame(3), 1_000, at(0));
        b.push(frame(1), 1_000, at(0));
        b.push(frame(2), 1_000, at(0));

        let order: Vec<u64> = std::iter::from_fn(|| b.pop(at(1_000_000)))
            .map(|f| f.sequence)
            .collect();

        assert_eq!(order, vec![1, 2, 3], "порядок кадров нарушен");
    }

    #[test]
    fn hopelessly_late_frame_is_dropped() {
        // Кадр, пришедший после того, как более новый уже показан,
        // нельзя выдавать: картинка дёрнулась бы назад.
        let mut b = JitterBuffer::new();

        b.push(frame(5), 1_000, at(0));
        assert!(b.pop(at(1_000_000)).is_some());

        b.push(frame(3), 1_000, at(1_000_000));
        assert_eq!(b.late_dropped(), 1);
        assert!(b.pop(at(2_000_000)).is_none(), "опоздавший кадр выдан");
    }

    #[test]
    fn frame_is_held_until_its_time() {
        // Основная механика: кадр не выдаётся раньше срока.
        let mut b = JitterBuffer::new();

        // Создаём джиттер, чтобы удержание стало ненулевым.
        for i in 0..30u64 {
            let delay = if i % 2 == 0 { 1_000 } else { 11_000 };
            b.push(frame(i), delay, at(i * 1_000));
            let _ = b.pop(at(i * 1_000 + 100_000));
        }

        let hold = b.hold();
        assert!(hold > Duration::ZERO);

        let arrival = at(10_000_000);
        b.push(frame(1000), 5_000, arrival);

        assert!(b.pop(arrival).is_none(), "кадр выдан раньше срока");
        assert!(
            b.pop(at(arrival.as_micros() + hold.as_micros() as u64))
                .is_some(),
            "кадр не выдан по наступлении срока"
        );
    }

    #[test]
    fn overflow_drops_oldest_not_newest() {
        // Потребитель встал. Свежие кадры ценнее устаревших — та же
        // логика, что везде в пайплайне (§5.3).
        let mut b = JitterBuffer::new();

        for i in 0..(MAX_QUEUED as u64 + 5) {
            b.push(frame(i), 1_000, at(i * 1_000));
        }

        assert_eq!(b.queued(), MAX_QUEUED);
        assert_eq!(b.overflow_dropped(), 5);

        // Самый свежий обязан уцелеть.
        let last = std::iter::from_fn(|| b.pop(at(100_000_000)))
            .map(|f| f.sequence)
            .last()
            .expect("очередь не пуста");
        assert_eq!(last, MAX_QUEUED as u64 + 4);
    }

    #[test]
    fn empty_buffer_yields_nothing() {
        let mut b = JitterBuffer::new();
        assert!(b.pop(at(1_000_000)).is_none());
        assert_eq!(b.queued(), 0);
        assert_eq!(b.average_held(), Duration::ZERO);
    }

    #[test]
    fn clear_resets_sequence_tracking() {
        // После смены разрешения нумерация начинается заново, и
        // старый `last_delivered` отбрасывал бы все новые кадры.
        let mut b = JitterBuffer::new();

        b.push(frame(100), 1_000, at(0));
        assert!(b.pop(at(1_000_000)).is_some());

        b.clear();
        b.push(frame(1), 1_000, at(1_000_000));

        assert_eq!(b.late_dropped(), 0, "кадр после clear сочтён опоздавшим");
        assert!(b.pop(at(2_000_000)).is_some());
    }
}
