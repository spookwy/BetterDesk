//! Нарезка кадра на фрагменты и обратная сборка.
//!
//! # Зачем это на этапе 1
//!
//! Кадр 1080p при 15 Мбит/с — это в среднем ~31 КБ, а ключевой кадр
//! легко даёт 200 КБ. В датаграм QUIC помещается ~1200 байт полезной
//! нагрузки, то есть кадр всегда идёт десятками фрагментов. Значит,
//! сборщик на приёме — обязательная часть пайплайна, а не деталь
//! этапа 3, и его поведение при потерях определяет, что увидит
//! пользователь на плохом канале.
//!
//! # Политика при потере фрагмента
//!
//! Неполный кадр **не восстанавливается и не ждётся** — он выбрасывается
//! целиком. Ретрансмит устаревшего кадра хуже его потери (CLAUDE.md
//! §5.3): пока переспрашиваем, приходит следующий кадр, и мы всё равно
//! показали бы старую картинку с лишней задержкой.
//!
//! Вместо ожидания приёмник сообщает о потере наверх, и хост
//! запрашивает Intra Refresh. Это ровно та схема, что будет на этапе 6.

use crate::error::{Result, TransportError};
use crate::packet::{FragmentHeader, PayloadKind, HEADER_SIZE};

/// Полезная нагрузка одного датаграма без учёта заголовка BetterDesk.
///
/// 1200 байт — консервативный размер, проходящий почти любой путь без
/// фрагментации IP: 1500 (типовой MTU Ethernet) минус запас на IPv6,
/// UDP и служебные заголовки QUIC. Брать больше — значит нарваться
/// на IP-фрагментацию, где потеря одного осколка убивает весь датаграм.
pub const DEFAULT_MAX_PAYLOAD: usize = 1200;

/// Предел числа фрагментов на кадр.
///
/// При 1200 байт на фрагмент это 4.8 МБ — заведомо больше любого
/// разумного кадра 1080p. Ограничение защищает от двух вещей сразу:
/// от попытки отправить мусорный «кадр» на гигабайты и от переполнения
/// 16-битного поля `count` в заголовке.
pub const MAX_FRAGMENTS: usize = 4096;

/// Нарезка кадра на фрагменты.
///
/// Владеет буфером и переиспользует его между кадрами: 60 раз в
/// секунду выделять память под десятки датаграмов — лишняя работа
/// в горячем пути.
#[derive(Debug)]
pub struct Fragmenter {
    max_payload: usize,
}

impl Fragmenter {
    /// Нарезка с размером полезной нагрузки по умолчанию.
    pub fn new() -> Self {
        Self::with_max_payload(DEFAULT_MAX_PAYLOAD)
    }

    /// Нарезка с заданным размером полезной нагрузки.
    ///
    /// # Паника
    ///
    /// Если размер нулевой. Это ошибка конфигурации, а не разбор
    /// недоверенных данных (CLAUDE.md §4.3.6).
    pub fn with_max_payload(max_payload: usize) -> Self {
        assert!(max_payload > 0, "размер полезной нагрузки должен быть > 0");
        Self { max_payload }
    }

    /// Размер полезной нагрузки одного фрагмента.
    pub fn max_payload(&self) -> usize {
        self.max_payload
    }

    /// Сколько фрагментов потребуется кадру такого размера.
    ///
    /// Пустой кадр даёт один фрагмент, а не ноль: получатель должен
    /// узнать о самом факте кадра.
    pub fn fragment_count(&self, bytes: usize) -> usize {
        if bytes == 0 {
            1
        } else {
            bytes.div_ceil(self.max_payload)
        }
    }

    /// Нарезать кадр, вызывая `emit` на каждый готовый датаграм.
    ///
    /// Форма с колбэком выбрана намеренно: она не выделяет `Vec<Vec<u8>>`
    /// на кадр. При 60 fps это 60 аллокаций вектора векторов в секунду
    /// на ровном месте.
    pub fn fragment<F>(
        &self,
        kind: PayloadKind,
        sequence: u64,
        keyframe: bool,
        captured_at_micros: u64,
        data: &[u8],
        mut emit: F,
    ) -> Result<usize>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        let count = self.fragment_count(data.len());
        if count > MAX_FRAGMENTS {
            return Err(TransportError::TooLarge {
                bytes: data.len(),
                limit: MAX_FRAGMENTS,
            });
        }

        let mut datagram = Vec::with_capacity(HEADER_SIZE + self.max_payload);

        for index in 0..count {
            let start = index * self.max_payload;
            let end = (start + self.max_payload).min(data.len());
            // Пустой кадр: единственный фрагмент с пустой нагрузкой.
            let chunk = data.get(start..end).unwrap_or(&[]);

            let header = FragmentHeader {
                kind,
                sequence,
                // Приведение безопасно: count проверен против
                // MAX_FRAGMENTS, а он много меньше u16::MAX.
                index: index as u16,
                count: count as u16,
                keyframe,
                // Одна метка на все фрагменты кадра: они описывают
                // один и тот же момент захвата.
                captured_at_micros,
            };

            datagram.clear();
            datagram.resize(HEADER_SIZE, 0);
            header.write_to(&mut datagram)?;
            datagram.extend_from_slice(chunk);

            emit(&datagram)?;
        }

        Ok(count)
    }
}

impl Default for Fragmenter {
    fn default() -> Self {
        Self::new()
    }
}

/// Собранный из фрагментов кадр.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    /// Вид полезной нагрузки.
    pub kind: PayloadKind,
    /// Номер кадра.
    pub sequence: u64,
    /// Был ли кадр ключевым.
    pub keyframe: bool,
    /// Когда кадр был захвачен на отправителе, в микросекундах его
    /// эпохи.
    ///
    /// Часы сторон не синхронизированы — вычитать это из своего
    /// времени напрямую нельзя. Смещение оценивает `ClockSync`.
    pub captured_at_micros: u64,
    /// Собранные байты.
    pub data: Vec<u8>,
}

/// Кадр в процессе сборки.
#[derive(Debug)]
struct Pending {
    sequence: u64,
    kind: PayloadKind,
    keyframe: bool,
    captured_at_micros: u64,
    count: u16,
    received: u16,
    /// Слоты фрагментов. `None` — фрагмент ещё не пришёл.
    ///
    /// Хранить именно слоты, а не конкатенацию по мере прихода,
    /// обязательно: датаграмы приходят не по порядку, и склейка
    /// в порядке прибытия дала бы перемешанный поток H.264.
    slots: Vec<Option<Vec<u8>>>,
}

impl Pending {
    fn new(header: &FragmentHeader) -> Self {
        let count = header.count as usize;
        let mut slots = Vec::new();
        slots.resize_with(count, || None);
        Self {
            sequence: header.sequence,
            kind: header.kind,
            keyframe: header.keyframe,
            captured_at_micros: header.captured_at_micros,
            count: header.count,
            received: 0,
            slots,
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.count
    }
}

/// Итог обработки одного датаграма приёмником.
#[derive(Debug, PartialEq, Eq)]
pub enum ReceiveOutcome {
    /// Кадр собран целиком.
    Frame(Box<ReassembledFrame>),
    /// Фрагмент принят, кадр ещё не полон.
    Pending,
    /// Датаграм отброшен, и почему.
    Dropped(DropReason),
}

/// Причина, по которой датаграм не пошёл в дело.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Фрагмент относится к кадру старше уже показанного.
    ///
    /// Обычная ситуация при переупорядочивании: пакет догнал нас,
    /// когда кадр уже вытеснен. Показывать его поздно.
    TooOld,
    /// Повторный фрагмент (дубликат или переотправка).
    Duplicate,
    /// Кадр вытеснен из сборщика более новым.
    ///
    /// Это и есть потеря: часть фрагментов не пришла, а ждать их
    /// дольше нельзя — новый кадр уже здесь.
    Incomplete,
}

/// Сборщик кадров из фрагментов.
///
/// Держит несколько кадров одновременно: датаграмы приходят не по
/// порядку, и хвост предыдущего кадра вполне может прийти после
/// головы следующего.
#[derive(Debug)]
pub struct Reassembler {
    pending: Vec<Pending>,
    capacity: usize,
    /// Номер последнего выданного наверх кадра.
    last_delivered: Option<u64>,
    /// Сколько кадров выброшено неполными.
    lost_frames: u64,
    /// Сколько фрагментов отброшено.
    dropped_fragments: u64,
}

impl Reassembler {
    /// Сборщик, удерживающий `capacity` незавершённых кадров.
    ///
    /// Два-три кадра — разумный предел: держать больше значит
    /// соглашаться показать кадр, устаревший на несколько кадров,
    /// а это прямо противоречит цели по задержке.
    ///
    /// # Паника
    ///
    /// Если `capacity` равна нулю (ошибка конфигурации, §4.3.6).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ёмкость сборщика должна быть положительной");
        Self {
            pending: Vec::with_capacity(capacity),
            capacity,
            last_delivered: None,
            lost_frames: 0,
            dropped_fragments: 0,
        }
    }

    /// Число кадров, выброшенных неполными.
    pub fn lost_frames(&self) -> u64 {
        self.lost_frames
    }

    /// Число отброшенных фрагментов.
    pub fn dropped_fragments(&self) -> u64 {
        self.dropped_fragments
    }

    /// Принять датаграм.
    ///
    /// # Ошибки
    ///
    /// [`TransportError::Malformed`] — датаграм не прошёл разбор. Это
    /// не повод рвать сессию: мусор мог прийти от кого угодно, а
    /// UDP-сокет не даёт гарантий отправителя до аутентификации (§8.5).
    pub fn accept(&mut self, datagram: &[u8]) -> Result<ReceiveOutcome> {
        let (header, payload) = FragmentHeader::parse(datagram)?;

        // Кадр, который мы уже показали или уже проехали. Инвариант
        // «номера растут» — наш собственный, так что сравнение честное.
        if let Some(last) = self.last_delivered {
            if header.sequence <= last {
                self.dropped_fragments += 1;
                return Ok(ReceiveOutcome::Dropped(DropReason::TooOld));
            }
        }

        let slot = match self
            .pending
            .iter()
            .position(|p| p.sequence == header.sequence)
        {
            Some(index) => index,
            None => {
                // Место под новый кадр. При переполнении вытесняется
                // самый старый: он неполон, а ждать его дольше — значит
                // копить задержку.
                if self.pending.len() >= self.capacity {
                    self.evict_oldest();
                }
                self.pending.push(Pending::new(&header));
                self.pending.len() - 1
            }
        };

        let pending = &mut self.pending[slot];

        // Число фрагментов должно совпадать у всех фрагментов кадра.
        // Расхождение означает либо подделку, либо коллизию номеров.
        if pending.count != header.count {
            self.dropped_fragments += 1;
            return Err(TransportError::Malformed(
                "число фрагментов расходится внутри кадра",
            ));
        }

        // Индекс проверен в parse: index < count == slots.len().
        let cell = &mut pending.slots[header.index as usize];
        if cell.is_some() {
            self.dropped_fragments += 1;
            return Ok(ReceiveOutcome::Dropped(DropReason::Duplicate));
        }
        *cell = Some(payload.to_vec());
        pending.received += 1;

        if !pending.is_complete() {
            return Ok(ReceiveOutcome::Pending);
        }

        let complete = self.pending.remove(slot);

        // Кадры, оставшиеся незавершёнными и старше выданного, уже
        // не пригодятся: их номер меньше, а мы отдали более новый.
        self.drop_older_than(complete.sequence);
        self.last_delivered = Some(complete.sequence);

        let mut data = Vec::with_capacity(
            complete
                .slots
                .iter()
                .map(|s| s.as_ref().map_or(0, Vec::len))
                .sum(),
        );
        for chunk in complete.slots.into_iter().flatten() {
            data.extend_from_slice(&chunk);
        }

        Ok(ReceiveOutcome::Frame(Box::new(ReassembledFrame {
            kind: complete.kind,
            sequence: complete.sequence,
            keyframe: complete.keyframe,
            captured_at_micros: complete.captured_at_micros,
            data,
        })))
    }

    /// Выбросить самый старый незавершённый кадр.
    fn evict_oldest(&mut self) {
        let Some(index) = self
            .pending
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.sequence)
            .map(|(i, _)| i)
        else {
            return;
        };
        let dropped = self.pending.remove(index);
        self.lost_frames += 1;
        self.dropped_fragments += dropped.received as u64;
    }

    /// Выбросить незавершённые кадры старше указанного номера.
    fn drop_older_than(&mut self, sequence: u64) {
        let mut lost = 0u64;
        let mut fragments = 0u64;
        self.pending.retain(|p| {
            if p.sequence < sequence {
                lost += 1;
                fragments += p.received as u64;
                false
            } else {
                true
            }
        });
        self.lost_frames += lost;
        self.dropped_fragments += fragments;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Нарезать кадр в вектор датаграмов — удобно для тестов.
    fn split(f: &Fragmenter, seq: u64, data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        f.fragment(PayloadKind::Video, seq, false, 0, data, |d| {
            out.push(d.to_vec());
            Ok(())
        })
        .expect("нарезка");
        out
    }

    #[test]
    fn roundtrip_of_a_multi_fragment_frame() {
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..350u32).map(|i| i as u8).collect();
        let datagrams = split(&f, 1, &data);
        assert_eq!(datagrams.len(), 4, "350 байт по 100 — четыре фрагмента");

        let mut r = Reassembler::new(2);
        for (i, d) in datagrams.iter().enumerate() {
            match r.accept(d).expect("приём") {
                ReceiveOutcome::Frame(frame) => {
                    assert_eq!(i, 3, "кадр обязан собраться на последнем фрагменте");
                    assert_eq!(frame.data, data);
                    assert_eq!(frame.sequence, 1);
                }
                ReceiveOutcome::Pending => assert!(i < 3),
                other => panic!("неожиданный итог: {other:?}"),
            }
        }
    }

    #[test]
    fn out_of_order_fragments_reassemble_correctly() {
        // Главная причина хранить слоты, а не склейку по приходу.
        let f = Fragmenter::with_max_payload(10);
        let data: Vec<u8> = (0..35u32).map(|i| i as u8).collect();
        let mut datagrams = split(&f, 7, &data);
        datagrams.reverse();

        let mut r = Reassembler::new(2);
        let mut result = None;
        for d in &datagrams {
            if let ReceiveOutcome::Frame(frame) = r.accept(d).expect("приём") {
                result = Some(frame);
            }
        }
        assert_eq!(result.expect("кадр собран").data, data);
    }

    #[test]
    fn empty_frame_produces_one_fragment() {
        let f = Fragmenter::with_max_payload(100);
        let datagrams = split(&f, 1, &[]);
        assert_eq!(datagrams.len(), 1);

        let mut r = Reassembler::new(2);
        match r.accept(&datagrams[0]).expect("приём") {
            ReceiveOutcome::Frame(frame) => assert!(frame.data.is_empty()),
            other => panic!("ожидался кадр, получено {other:?}"),
        }
    }

    #[test]
    fn exact_multiple_does_not_emit_empty_tail() {
        // 200 байт по 100 — ровно два фрагмента, а не три.
        let f = Fragmenter::with_max_payload(100);
        assert_eq!(split(&f, 1, &[0u8; 200]).len(), 2);
    }

    #[test]
    fn incomplete_frame_is_dropped_not_awaited() {
        // Ключевое поведение: потеря фрагмента не должна тормозить
        // следующий кадр (CLAUDE.md §5.3).
        let f = Fragmenter::with_max_payload(10);
        let lossy = split(&f, 1, &[1u8; 35]);
        let good = split(&f, 2, &[2u8; 35]);

        let mut r = Reassembler::new(2);
        // У первого кадра теряем последний фрагмент.
        for d in &lossy[..3] {
            assert_eq!(r.accept(d).expect("приём"), ReceiveOutcome::Pending);
        }
        // Второй кадр приходит целиком и обязан быть выдан.
        let mut delivered = None;
        for d in &good {
            if let ReceiveOutcome::Frame(frame) = r.accept(d).expect("приём") {
                delivered = Some(frame);
            }
        }
        let frame = delivered.expect("второй кадр обязан пройти");
        assert_eq!(frame.sequence, 2);
        assert_eq!(r.lost_frames(), 1, "первый кадр засчитан потерянным");
    }

    #[test]
    fn late_fragment_of_delivered_frame_is_rejected() {
        let f = Fragmenter::with_max_payload(10);
        let first = split(&f, 1, &[1u8; 15]);
        let second = split(&f, 2, &[2u8; 15]);

        let mut r = Reassembler::new(3);
        for d in &first {
            let _ = r.accept(d).expect("приём");
        }
        for d in &second {
            let _ = r.accept(d).expect("приём");
        }
        // Опоздавший дубликат первого кадра.
        assert_eq!(
            r.accept(&first[0]).expect("приём"),
            ReceiveOutcome::Dropped(DropReason::TooOld)
        );
    }

    #[test]
    fn duplicate_fragment_is_detected() {
        let f = Fragmenter::with_max_payload(10);
        let datagrams = split(&f, 1, &[1u8; 25]);

        let mut r = Reassembler::new(2);
        assert_eq!(
            r.accept(&datagrams[0]).expect("приём"),
            ReceiveOutcome::Pending
        );
        assert_eq!(
            r.accept(&datagrams[0]).expect("приём"),
            ReceiveOutcome::Dropped(DropReason::Duplicate),
            "повтор не должен считаться новым фрагментом"
        );
    }

    #[test]
    fn capacity_overflow_evicts_oldest() {
        let f = Fragmenter::with_max_payload(10);
        let mut r = Reassembler::new(2);

        // Три незавершённых кадра при ёмкости 2.
        for seq in 1..=3u64 {
            let datagrams = split(&f, seq, &[0u8; 35]);
            let _ = r.accept(&datagrams[0]).expect("приём");
        }
        assert_eq!(r.lost_frames(), 1, "самый старый кадр вытеснен");
    }

    #[test]
    fn capture_timestamp_survives_fragmentation_and_reassembly() {
        // Метка времени — единственное, что позволяет измерить
        // задержку между машинами (находка 40). Если она теряется
        // по пути, отказа не будет: просто задержка станет
        // бессмысленной, и это заметят не сразу.
        //
        // Кадр берётся многофрагментный намеренно: метка кладётся в
        // каждый фрагмент, и сборщик должен взять её из первого
        // пришедшего, а не потерять при склейке.
        const CAPTURED: u64 = 0x0011_2233_4455_6677;

        let f = Fragmenter::with_max_payload(4);
        let mut r = Reassembler::new(2);
        let payload = vec![0xABu8; 17]; // 5 фрагментов
        let mut datagrams = Vec::new();

        f.fragment(PayloadKind::Video, 9, false, CAPTURED, &payload, |d| {
            datagrams.push(d.to_vec());
            Ok(())
        })
        .expect("нарезка");

        assert!(datagrams.len() > 1, "нужен многофрагментный кадр");

        let mut assembled = None;
        for d in &datagrams {
            if let Ok(ReceiveOutcome::Frame(frame)) = r.accept(d) {
                assembled = Some(frame);
            }
        }

        let frame = assembled.expect("кадр должен собраться");
        assert_eq!(
            frame.captured_at_micros, CAPTURED,
            "метка времени не пережила путь через транспорт"
        );
        assert_eq!(frame.data, payload, "заодно и байты");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let f = Fragmenter::with_max_payload(1);
        let err = f
            .fragment(
                PayloadKind::Video,
                1,
                false,
                0,
                &vec![0u8; MAX_FRAGMENTS + 1],
                |_| Ok(()),
            )
            .expect_err("кадр обязан быть отвергнут");
        assert!(matches!(err, TransportError::TooLarge { .. }));
    }

    #[test]
    fn mismatched_fragment_count_is_rejected() {
        // Подделанный пакет с тем же номером кадра, но другим count.
        let f = Fragmenter::with_max_payload(10);
        let datagrams = split(&f, 1, &[0u8; 35]);
        let mut r = Reassembler::new(2);
        let _ = r.accept(&datagrams[0]).expect("приём");

        let mut forged = datagrams[1].clone();
        forged[14..16].copy_from_slice(&9u16.to_be_bytes());
        assert!(r.accept(&forged).is_err());
    }

    #[test]
    fn keyframe_flag_survives_the_roundtrip() {
        let f = Fragmenter::new();
        let mut out = Vec::new();
        f.fragment(PayloadKind::Video, 1, true, 12_345, b"abc", |d| {
            out.push(d.to_vec());
            Ok(())
        })
        .expect("нарезка");

        let mut r = Reassembler::new(2);
        match r.accept(&out[0]).expect("приём") {
            ReceiveOutcome::Frame(frame) => {
                assert!(frame.keyframe, "признак ключевого кадра обязан дойти");
                assert_eq!(frame.data, b"abc");
            }
            other => panic!("ожидался кадр, получено {other:?}"),
        }
    }
}
