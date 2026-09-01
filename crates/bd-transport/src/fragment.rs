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
use crate::fec::{encode_parity, parity_count, recover, FecPolicy};
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
        emit: F,
    ) -> Result<usize>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        self.fragment_with_fec(
            kind,
            sequence,
            keyframe,
            captured_at_micros,
            data,
            FecPolicy::Off,
            emit,
        )
    }

    /// Нарезать кадр, добавив паритетные фрагменты по политике `fec`.
    ///
    /// # Почему паритет считается от выровненных кусков
    ///
    /// Reed-Solomon требует кусков одинаковой длины, а последний
    /// фрагмент кадра почти всегда короче. Поэтому для расчёта
    /// паритета хвост дополняется нулями — но **в сеть уходит
    /// исходная длина**: дополнение не передаётся, приёмник
    /// восстановит его сам, зная `max_payload`.
    ///
    /// Иначе каждый кадр платил бы за выравнивание лишними байтами,
    /// а на мелких кадрах это удвоило бы трафик.
    // Восемь аргументов вместо семи. Заворачивать их в структуру
    // было бы хуже: параметры кадра (вид, номер, ключевой, метка)
    // задаются на каждый вызов и все обязательны, а структура ради
    // одного вызывающего добавила бы имя, которое нужно помнить, без
    // единой проверки взамен. `fragment` рядом принимает те же семь.
    #[allow(clippy::too_many_arguments)]
    pub fn fragment_with_fec<F>(
        &self,
        kind: PayloadKind,
        sequence: u64,
        keyframe: bool,
        captured_at_micros: u64,
        data: &[u8],
        fec: FecPolicy,
        mut emit: F,
    ) -> Result<usize>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        let data_count = self.fragment_count(data.len());
        let parity = parity_count(data_count, fec.redundancy_percent());
        let count = data_count + parity;

        if count > MAX_FRAGMENTS {
            return Err(TransportError::TooLarge {
                bytes: data.len(),
                limit: MAX_FRAGMENTS,
            });
        }

        let mut datagram = Vec::with_capacity(HEADER_SIZE + self.max_payload);

        let make_header = |index: usize| FragmentHeader {
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
            parity: parity as u8,
        };

        // Собрать датаграм данных в буфер. Функция, а не замыкание:
        // замыкание удерживало бы `datagram` заимствованным на всё
        // время цикла, а тот же буфер нужен и паритетным фрагментам.
        let write_data = |datagram: &mut Vec<u8>, index: usize| -> Result<()> {
            let start = index * self.max_payload;
            let end = (start + self.max_payload).min(data.len());
            // Пустой кадр: единственный фрагмент с пустой нагрузкой.
            let chunk = data.get(start..end).unwrap_or(&[]);

            datagram.clear();
            datagram.resize(HEADER_SIZE, 0);
            make_header(index).write_to(datagram)?;
            datagram.extend_from_slice(chunk);
            Ok(())
        };

        if parity == 0 {
            for index in 0..data_count {
                write_data(&mut datagram, index)?;
                emit(&datagram)?;
            }
            return Ok(count);
        }

        // Куски для FEC: те же данные, но выровненные нулями до общей
        // длины. Выравнивание живёт только здесь и в приёмнике — в
        // сеть уходят исходные длины.
        let shard_len = shard_len_for(self.max_payload);
        let mut shards = Vec::with_capacity(data_count);
        for index in 0..data_count {
            let start = index * self.max_payload;
            let end = (start + self.max_payload).min(data.len());
            let mut shard = data.get(start..end).unwrap_or(&[]).to_vec();
            shard.resize(shard_len, 0);
            shards.push(shard);
        }
        let parity_shards = encode_parity(&shards, parity)?;

        // Паритет ПЕРЕМЕЖАЕТСЯ с данными, а не идёт следом за ними.
        //
        // # Почему это не косметика
        //
        // Первая версия слала весь паритет после всех данных, и это
        // выглядело естественно. Замер показал обратное: при 3 %
        // потерь FEC не улучшил картину, а ухудшил её — 45.7 %
        // потерянных кадров против 38.8 % без него.
        //
        // Диагностический счётчик назвал причину прямо: **499 кадров
        // выброшено при живом паритете** против 45 восстановленных.
        // Кадр, у которого потерян ранний фрагмент, ждёт паритета —
        // а тот идёт последним, и за это время успевает собраться
        // СЛЕДУЮЩИЙ кадр. Сборщик отдаёт его наверх и вычищает всё
        // с меньшим номером (`drop_older_than`), убивая кадр за шаг
        // до спасения.
        //
        // Перемежение ставит паритет в середину потока кадра: он
        // приходит раньше, чем следующий кадр успевает собраться
        // целиком. Порядок в сети не гарантирован, но порядок
        // ОТПРАВКИ определяет типичный порядок прихода, и этого
        // достаточно.
        //
        // Индексы при этом не меняются: в заголовке у паритета
        // по-прежнему `data_count..count`. Перемежается только
        // очерёдность отправки, а состав кадра остаётся прежним —
        // иначе приёмник пришлось бы учить новой раскладке.
        let step = data_count.div_ceil(parity + 1).max(1);
        let mut next_parity = 0usize;
        for index in 0..data_count {
            write_data(&mut datagram, index)?;
            emit(&datagram)?;

            // После каждых `step` фрагментов данных — один паритетный.
            if next_parity < parity && (index + 1) % step == 0 {
                let shard = &parity_shards[next_parity];
                datagram.clear();
                datagram.resize(HEADER_SIZE, 0);
                make_header(data_count + next_parity).write_to(&mut datagram)?;
                // Настоящая длина кадра — в начале каждого паритетного
                // фрагмента.
                //
                // Без неё восстановленный кадр нечем обрезать: куски
                // выровнены нулями, и длину задавал последний фрагмент
                // данных — ровно тот, который мог потеряться. Дописать
                // хвост нулей в поток H.264 значит отдать декодеру мусор
                // без единой ошибки.
                datagram.extend_from_slice(&(data.len() as u32).to_be_bytes());
                datagram.extend_from_slice(shard);
                emit(&datagram)?;
                next_parity += 1;
            }
        }

        // Остаток паритета, если данных не хватило на все вставки.
        while next_parity < parity {
            let shard = &parity_shards[next_parity];
            datagram.clear();
            datagram.resize(HEADER_SIZE, 0);
            make_header(data_count + next_parity).write_to(&mut datagram)?;
            datagram.extend_from_slice(&(data.len() as u32).to_be_bytes());
            datagram.extend_from_slice(shard);
            emit(&datagram)?;
            next_parity += 1;
        }

        Ok(count)
    }
}

/// Длина куска для Reed-Solomon при заданном `max_payload`.
///
/// Библиотека требует чётной длины. `max_payload` у нас 1200, но
/// константа проверяется явно: нечётное значение дало бы ошибку не
/// здесь, а в глубине FEC, где её труднее объяснить.
fn shard_len_for(max_payload: usize) -> usize {
    if max_payload.is_multiple_of(2) {
        max_payload
    } else {
        max_payload + 1
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
    /// Слоты фрагментов данных. `None` — фрагмент ещё не пришёл.
    ///
    /// Хранить именно слоты, а не конкатенацию по мере прихода,
    /// обязательно: датаграмы приходят не по порядку, и склейка
    /// в порядке прибытия дала бы перемешанный поток H.264.
    slots: Vec<Option<Vec<u8>>>,
    /// Слоты паритетных фрагментов (FEC). Пусто, если избыточности нет.
    parity_slots: Vec<Option<Vec<u8>>>,
    /// Сколько фрагментов данных уже пришло.
    ///
    /// Отдельно от `received`, который считает все: полнота данных и
    /// полнота вместе с паритетом — разные условия. Кадр готов, когда
    /// собраны **данные**; паритет нужен, только пока их не хватает.
    data_received: u16,
    /// Настоящая длина кадра, если её сообщил паритетный фрагмент.
    declared_len: Option<u32>,
}

impl Pending {
    fn new(header: &FragmentHeader) -> Self {
        let data_count = header.data_count() as usize;
        let parity = header.parity as usize;
        let mut slots = Vec::new();
        slots.resize_with(data_count, || None);
        let mut parity_slots = Vec::new();
        parity_slots.resize_with(parity, || None);
        Self {
            sequence: header.sequence,
            kind: header.kind,
            keyframe: header.keyframe,
            captured_at_micros: header.captured_at_micros,
            count: header.count,
            received: 0,
            slots,
            parity_slots,
            data_received: 0,
            declared_len: None,
        }
    }

    /// Все ли фрагменты **данных** на месте.
    fn is_complete(&self) -> bool {
        self.data_received as usize == self.slots.len()
    }

    /// Хватает ли пришедшего, чтобы восстановить недостающее.
    ///
    /// Reed-Solomon чинит стирания, пока суммарно пришло не меньше
    /// кусков, чем было данных.
    fn can_recover(&self) -> bool {
        if self.parity_slots.is_empty() || self.is_complete() {
            return false;
        }
        let present_parity = self.parity_slots.iter().filter(|s| s.is_some()).count();
        self.data_received as usize + present_parity >= self.slots.len()
    }

    /// Попытаться восстановить недостающие фрагменты данных.
    ///
    /// Возвращает `true`, если после этого кадр стал полным.
    fn try_recover(&mut self) -> bool {
        let Ok(Some(restored)) = recover(&self.slots, &self.parity_slots) else {
            return false;
        };
        for (slot, value) in self.slots.iter_mut().zip(restored) {
            if slot.is_none() {
                *slot = Some(value);
            }
        }
        self.data_received = self.slots.len() as u16;
        true
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

/// Сборка кадров одного вида нагрузки.
///
/// # Почему состояние раздельное, а не общее
///
/// Пока каждый вид ехал своим соединением на своём порту (находка 56),
/// разделять было нечего: в очереди лежали только видеокадры, только
/// события ввода или только позиции курсора.
///
/// В одном соединении это перестаёт быть верным. Нумерация у каждого
/// вида своя и растёт со своей скоростью: видео идёт 60 кадрами в
/// секунду, ввод — сотнями событий. Общий `last_delivered` означал бы,
/// что вид с быстрой нумерацией объявляет пакеты медленного
/// устаревшими — и молча их выбрасывает, потому что `TooOld` не
/// ошибка, а норма при переупорядочивании.
///
/// Ошибка была бы **тихой**: ни отказа, ни искажённых байтов. Просто
/// курсор перестал бы двигаться, а клавиатура — печатать, и искать
/// причину пришлось бы в коде ввода, который исправен.
#[derive(Debug)]
struct KindState {
    pending: Vec<Pending>,
    capacity: usize,
    /// Номер последнего выданного наверх кадра этого вида.
    last_delivered: Option<u64>,
    lost_frames: u64,
    dropped_fragments: u64,
    /// Кадров, собранных благодаря FEC.
    ///
    /// Без этого счётчика «FEC работает» неотличимо от «потерь не
    /// было» — ровно урок находки 52, где число подавленных запросов
    /// пришлось печатать по той же причине.
    recovered_frames: u64,
    /// Кадров выброшено, хотя у них был заведён паритет.
    evicted_with_parity: u64,
}

impl KindState {
    fn new(capacity: usize) -> Self {
        Self {
            pending: Vec::with_capacity(capacity),
            capacity,
            last_delivered: None,
            lost_frames: 0,
            dropped_fragments: 0,
            recovered_frames: 0,
            evicted_with_parity: 0,
        }
    }

    /// Принять уже разобранный фрагмент.
    ///
    /// Заголовок приходит разобранным, а не сырым датаграмом: вид
    /// нагрузки нужно прочитать **до** выбора состояния, и разбирать
    /// его дважды незачем.
    fn accept(&mut self, header: FragmentHeader, payload: &[u8]) -> Result<ReceiveOutcome> {
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

        // Паритетный фрагмент идёт в свои слоты, а не в данные:
        // сложи их вместе — и сборка склеила бы избыточность в поток
        // H.264 как обычные байты.
        if header.is_parity() {
            let offset = (header.index - header.data_count()) as usize;
            // Индекс проверен в parse (index < count) и приведён к
            // паритетной части вычитанием data_count, но состав кадра
            // мог быть объявлен иначе первым пришедшим фрагментом.
            let Some(cell) = pending.parity_slots.get_mut(offset) else {
                self.dropped_fragments += 1;
                return Err(TransportError::Malformed(
                    "индекс паритета вне объявленного состава",
                ));
            };
            if cell.is_some() {
                self.dropped_fragments += 1;
                return Ok(ReceiveOutcome::Dropped(DropReason::Duplicate));
            }
            // Первые четыре байта паритета — настоящая длина кадра.
            if payload.len() < 4 {
                self.dropped_fragments += 1;
                return Err(TransportError::Malformed("паритет короче длины кадра"));
            }
            let (len_bytes, shard) = payload.split_at(4);
            let declared = u32::from_be_bytes(
                len_bytes
                    .try_into()
                    .map_err(|_| TransportError::Malformed("длина кадра в паритете"))?,
            );
            pending.declared_len = Some(declared);
            *cell = Some(shard.to_vec());
            pending.received += 1;
        } else {
            let cell = &mut pending.slots[header.index as usize];
            if cell.is_some() {
                self.dropped_fragments += 1;
                return Ok(ReceiveOutcome::Dropped(DropReason::Duplicate));
            }
            *cell = Some(payload.to_vec());
            pending.received += 1;
            pending.data_received += 1;
        }

        if !pending.is_complete() {
            // Данных не хватает — но, возможно, хватает паритета.
            //
            // Восстановление пробуется здесь, а не при вытеснении:
            // кадр должен уходить наверх сразу, как только его можно
            // собрать. Ждать до вытеснения значило бы добавить к
            // задержке время жизни кадра в очереди — то есть лечить
            // потери ценой того, ради чего весь проект.
            if !(pending.can_recover() && pending.try_recover()) {
                return Ok(ReceiveOutcome::Pending);
            }
            self.recovered_frames += 1;
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
        for chunk in complete.slots.iter().flatten() {
            data.extend_from_slice(chunk);
        }

        // Обрезать хвост выравнивания, если кадр собран с участием FEC.
        //
        // Восстановленные куски приходят выровненными нулями до общей
        // длины — иначе Reed-Solomon не работает. Настоящую длину
        // сообщает паритетный фрагмент, и без обрезки в поток H.264
        // ушёл бы хвост нулей: декодер не выдал бы ошибки, а показал
        // бы порчу.
        //
        // Урезаем только вниз: объявленная длина больше собранной
        // означала бы либо подделку, либо расхождение состава, и
        // растягивать данные под неё нельзя.
        if let Some(declared) = complete.declared_len {
            let declared = declared as usize;
            if declared < data.len() {
                data.truncate(declared);
            }
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
        let mut had_parity = 0u64;
        self.pending.retain(|p| {
            if p.sequence < sequence {
                lost += 1;
                fragments += p.received as u64;
                if !p.parity_slots.is_empty() {
                    had_parity += 1;
                }
                false
            } else {
                true
            }
        });
        self.lost_frames += lost;
        self.dropped_fragments += fragments;
        self.evicted_with_parity += had_parity;
    }
}

/// Сборщик кадров из фрагментов.
///
/// Держит несколько кадров одновременно: датаграмы приходят не по
/// порядку, и хвост предыдущего кадра вполне может прийти после
/// головы следующего.
///
/// # Виды нагрузки не смешиваются
///
/// Каждый [`PayloadKind`] собирается независимо: своя очередь
/// незавершённых кадров, свой номер последнего выданного, своя
/// ёмкость. Это то, что позволяет вести видео, ввод и курсор **по
/// одному соединению** — без чего не пробить NAT одним портом (§5.4).
///
/// Счётчики потерь при этом общие: пайплайну и оверлею нужна одна
/// цифра «сколько потеряно», а не три. Разбивка доступна отдельно
/// через [`Reassembler::lost_frames_of`].
#[derive(Debug)]
pub struct Reassembler {
    /// Состояния по видам, заводятся по мере появления.
    ///
    /// Вектор пар, а не `HashMap`: видов четыре, и линейный поиск по
    /// четырём элементам дешевле хеширования. Заодно структура
    /// остаётся без аллокации, пока не пришёл первый датаграм.
    kinds: Vec<(PayloadKind, KindState)>,
    capacity: usize,
}

impl Reassembler {
    /// Сборщик, удерживающий `capacity` незавершённых кадров **на
    /// каждый вид нагрузки**.
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
            kinds: Vec::new(),
            capacity,
        }
    }

    /// Число кадров, выброшенных неполными — по всем видам.
    pub fn lost_frames(&self) -> u64 {
        self.kinds.iter().map(|(_, s)| s.lost_frames).sum()
    }

    /// Число отброшенных фрагментов — по всем видам.
    pub fn dropped_fragments(&self) -> u64 {
        self.kinds.iter().map(|(_, s)| s.dropped_fragments).sum()
    }

    /// Число кадров, собранных благодаря FEC.
    ///
    /// Печатать обязательно: без него «FEC включён» неотличимо от
    /// «FEC работает». Ноль восстановлений при ненулевых потерях
    /// означает, что избыточности не хватает, а не что всё хорошо
    /// (находки 26 и 52).
    pub fn recovered_frames(&self) -> u64 {
        self.kinds.iter().map(|(_, s)| s.recovered_frames).sum()
    }

    /// Кадров выброшено, хотя под них уже был заведён паритет.
    ///
    /// Диагностика, а не метрика качества. Большое число здесь
    /// означает, что кадры не доживают до своего паритета: их
    /// вытесняет более новый кадр, собравшийся первым. То есть FEC
    /// физически способен был помочь, но не успел.
    ///
    /// Заведён потому, что без него «FEC не помогает» неотличимо от
    /// «FEC не успевает», а это разные диагнозы с разным лечением.
    pub fn evicted_with_parity(&self) -> u64 {
        self.kinds.iter().map(|(_, s)| s.evicted_with_parity).sum()
    }

    /// Число кадров, выброшенных неполными у одного вида.
    ///
    /// Нужно там, где потери видео и потери ввода означают разное:
    /// потерянный кадр — это артефакт на доли секунды, потерянное
    /// отпускание клавиши — залипание навсегда (§0.1, этап 2).
    pub fn lost_frames_of(&self, kind: PayloadKind) -> u64 {
        self.state_of(kind).map_or(0, |s| s.lost_frames)
    }

    fn state_of(&self, kind: PayloadKind) -> Option<&KindState> {
        self.kinds.iter().find(|(k, _)| *k == kind).map(|(_, s)| s)
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

        // Состояние вида заводится по первому пришедшему датаграму.
        //
        // Заводить все четыре заранее было бы проще, но неверно:
        // тогда `lost_frames_of` для вида, которого в сессии нет
        // вовсе, отвечал бы нулём наравне с видом, который идёт без
        // потерь. Различать «не было» и «было и дошло» полезно.
        let index = match self.kinds.iter().position(|(k, _)| *k == header.kind) {
            Some(index) => index,
            None => {
                self.kinds
                    .push((header.kind, KindState::new(self.capacity)));
                self.kinds.len() - 1
            }
        };

        self.kinds[index].1.accept(header, payload)
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

    /// Нарезать кадр заданного вида — для проверок смешения потоков.
    fn split_kind(f: &Fragmenter, kind: PayloadKind, seq: u64, data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        f.fragment(kind, seq, false, 0, data, |d| {
            out.push(d.to_vec());
            Ok(())
        })
        .expect("нарезка");
        out
    }

    #[test]
    fn fast_stream_does_not_starve_a_slow_one() {
        // Тот самый дефект, ради которого состояние разделено
        // (находка 56). Видео уходит далеко вперёд по нумерации, и
        // при общем `last_delivered` следующее событие ввода было бы
        // объявлено устаревшим и молча выброшено.
        //
        // «Молча» здесь ключевое: `TooOld` — не ошибка, а норма при
        // переупорядочивании, поэтому в логе не появилось бы ничего.
        let f = Fragmenter::with_max_payload(100);
        let mut r = Reassembler::new(3);

        // Видео разгоняется до сотого кадра.
        for seq in 1..=100u64 {
            for d in &split_kind(&f, PayloadKind::Video, seq, b"video") {
                let _ = r.accept(d).expect("приём видео");
            }
        }

        // Ввод только начинается — его первый пакет с номером 1.
        let input = split_kind(&f, PayloadKind::Input, 1, b"key");
        match r.accept(&input[0]).expect("приём ввода") {
            ReceiveOutcome::Frame(frame) => {
                assert_eq!(frame.kind, PayloadKind::Input);
                assert_eq!(frame.data, b"key");
            }
            other => {
                panic!("событие ввода обязано пройти при любом номере видео, получено {other:?}")
            }
        }
    }

    #[test]
    fn kinds_reassemble_independently_when_interleaved() {
        // Фрагменты трёх видов перемешаны в одной очереди — ровно то,
        // что происходит в одном QUIC-соединении. Каждый вид обязан
        // собраться целиком и не подобрать чужие байты.
        let f = Fragmenter::with_max_payload(4);
        let video: Vec<u8> = (0..14u8).collect();
        let input: Vec<u8> = (100..109u8).collect();
        let cursor: Vec<u8> = (200..206u8).collect();

        let mut mixed = Vec::new();
        let streams = [
            split_kind(&f, PayloadKind::Video, 5, &video),
            split_kind(&f, PayloadKind::Input, 5, &input),
            split_kind(&f, PayloadKind::Control, 5, &cursor),
        ];
        // Чередование по одному фрагменту из каждого потока.
        let longest = streams.iter().map(Vec::len).max().expect("потоки есть");
        for i in 0..longest {
            for s in &streams {
                if let Some(d) = s.get(i) {
                    mixed.push(d.clone());
                }
            }
        }

        let mut r = Reassembler::new(3);
        let mut got: Vec<(PayloadKind, Vec<u8>)> = Vec::new();
        for d in &mixed {
            if let ReceiveOutcome::Frame(frame) = r.accept(d).expect("приём") {
                got.push((frame.kind, frame.data));
            }
        }

        assert_eq!(got.len(), 3, "каждый вид обязан собраться");
        for (kind, expected) in [
            (PayloadKind::Video, &video),
            (PayloadKind::Input, &input),
            (PayloadKind::Control, &cursor),
        ] {
            let actual = got
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, d)| d)
                .unwrap_or_else(|| panic!("вид {kind:?} не собран"));
            assert_eq!(actual, expected, "байты вида {kind:?} перепутаны");
        }
    }

    #[test]
    fn same_sequence_in_different_kinds_is_not_a_duplicate() {
        // Нумерация у каждого вида своя и начинается с нуля, поэтому
        // совпадение номеров — норма, а не повтор. Общее состояние
        // сочло бы второй пакет дубликатом и выбросило.
        let f = Fragmenter::with_max_payload(100);
        let mut r = Reassembler::new(3);

        let video = split_kind(&f, PayloadKind::Video, 0, b"V");
        let input = split_kind(&f, PayloadKind::Input, 0, b"I");

        match r.accept(&video[0]).expect("приём") {
            ReceiveOutcome::Frame(frame) => assert_eq!(frame.data, b"V"),
            other => panic!("ожидался кадр, получено {other:?}"),
        }
        match r.accept(&input[0]).expect("приём") {
            ReceiveOutcome::Frame(frame) => assert_eq!(frame.data, b"I"),
            other => panic!("тот же номер у другого вида — не дубликат, получено {other:?}"),
        }
    }

    #[test]
    fn losses_are_counted_per_kind_and_in_total() {
        // Потеря кадра видео не должна выглядеть как потеря ввода:
        // артефакт на доли секунды и залипшая клавиша лечатся
        // по-разному, и одна цифра на двоих скрыла бы, что именно
        // теряется.
        let f = Fragmenter::with_max_payload(10);
        let mut r = Reassembler::new(2);

        // Видео: три кадра подряд неполными — вытесняются.
        for seq in 1..=3u64 {
            let d = split_kind(&f, PayloadKind::Video, seq, &[0u8; 35]);
            let _ = r.accept(&d[0]).expect("приём");
        }
        // Ввод идёт целиком.
        for seq in 1..=2u64 {
            for d in &split_kind(&f, PayloadKind::Input, seq, b"ok") {
                let _ = r.accept(d).expect("приём");
            }
        }

        assert_eq!(
            r.lost_frames_of(PayloadKind::Video),
            1,
            "видео потеряло кадр"
        );
        assert_eq!(
            r.lost_frames_of(PayloadKind::Input),
            0,
            "ввод прошёл без потерь"
        );
        assert_eq!(r.lost_frames(), 1, "итог сходится с разбивкой");
    }

    #[test]
    fn unseen_kind_reports_no_losses() {
        // «Вида не было» и «вид шёл без потерь» дают одинаковый ноль,
        // и это осознанно: различать их важно при чтении отчёта, а не
        // в коде. Проверка фиксирует, что обращение к незаведённому
        // виду не паникует и не заводит состояние на пустом месте.
        let r = Reassembler::new(2);
        assert_eq!(r.lost_frames_of(PayloadKind::Audio), 0);
        assert_eq!(r.lost_frames(), 0);
    }

    /// Нарезать кадр с FEC — как `split`, но с избыточностью.
    fn split_fec(f: &Fragmenter, seq: u64, data: &[u8], percent: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        f.fragment_with_fec(
            PayloadKind::Video,
            seq,
            // keyframe как у `split`: сравнение форматов должно
            // расходиться только из-за FEC, а не из-за флага.
            false,
            0,
            data,
            FecPolicy::Fixed(percent),
            |d| {
                out.push(d.to_vec());
                Ok(())
            },
        )
        .expect("нарезка с FEC");
        out
    }

    #[test]
    fn fec_adds_parity_fragments() {
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();

        let plain = split(&f, 1, &data);
        let with_fec = split_fec(&f, 1, &data, 20);

        assert_eq!(plain.len(), 10, "1000 байт по 100 — десять фрагментов");
        assert_eq!(with_fec.len(), 12, "десять данных плюс два паритетных");
    }

    #[test]
    fn fec_off_is_byte_identical_to_plain() {
        // FEC выключен — формат обязан совпасть с прежним до байта.
        // Иначе сторона старой сборки перестала бы понимать новую,
        // а обновляются стороны не одновременно.
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
        assert_eq!(split(&f, 7, &data), split_fec(&f, 7, &data, 0));
    }

    #[test]
    fn recovers_frame_from_lost_fragment() {
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let datagrams = split_fec(&f, 1, &data, 20);

        let mut r = Reassembler::new(2);
        let mut frame = None;
        // Теряем третий фрагмент данных — тот, что без FEC убил бы кадр.
        for (i, d) in datagrams.iter().enumerate() {
            if i == 2 {
                continue;
            }
            if let ReceiveOutcome::Frame(f) = r.accept(d).expect("приём") {
                frame = Some(f);
            }
        }

        let frame = frame.expect("кадр обязан собраться благодаря FEC");
        assert_eq!(frame.data, data, "восстановленные байты обязаны совпасть");
        assert_eq!(r.recovered_frames(), 1, "восстановление должно считаться");
        assert_eq!(r.lost_frames(), 0);
    }

    #[test]
    fn recovers_when_last_fragment_is_lost() {
        // Последний фрагмент данных короче остальных, и именно он
        // задаёт длину кадра. Его потеря — худший случай: длину
        // приходится брать из паритета, а не из данных.
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..1050u32).map(|i| (i % 251) as u8).collect();
        let datagrams = split_fec(&f, 1, &data, 20);

        let mut r = Reassembler::new(2);
        let mut frame = None;
        // 1050 байт по 100 — одиннадцать фрагментов, последний в 50 байт.
        for (i, d) in datagrams.iter().enumerate() {
            if i == 10 {
                continue;
            }
            if let ReceiveOutcome::Frame(f) = r.accept(d).expect("приём") {
                frame = Some(f);
            }
        }

        let frame = frame.expect("кадр обязан собраться");
        assert_eq!(
            frame.data.len(),
            data.len(),
            "длина обязана восстановиться, а не остаться выровненной"
        );
        assert_eq!(frame.data, data);
    }

    #[test]
    fn gives_up_when_losses_exceed_parity() {
        // Проверка обязана уметь отвечать «нет». FEC, который всегда
        // «восстанавливает», отдал бы наверх мусор без единой ошибки.
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let datagrams = split_fec(&f, 1, &data, 20); // два паритетных

        let mut r = Reassembler::new(2);
        let mut frames = 0;
        for (i, d) in datagrams.iter().enumerate() {
            // Три потери при двух паритетных — арифметически неподъёмно.
            if i == 1 || i == 4 || i == 7 {
                continue;
            }
            if let ReceiveOutcome::Frame(_) = r.accept(d).expect("приём") {
                frames += 1;
            }
        }
        assert_eq!(frames, 0, "кадр не должен собраться");
        assert_eq!(r.recovered_frames(), 0);
    }

    #[test]
    fn parity_loss_alone_does_not_break_the_frame() {
        // Паритет теряется наравне с данными. Если данные целы,
        // потеря избыточности не должна значить ничего.
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let datagrams = split_fec(&f, 1, &data, 20);

        let mut r = Reassembler::new(2);
        let mut frame = None;
        for d in datagrams.iter() {
            // Паритетные отбираются по заголовку, а не по позиции:
            // они перемежены с данными, и «последние два» — уже не они.
            let (header, _) = FragmentHeader::parse(d).expect("разбор");
            if header.is_parity() {
                continue;
            }
            if let ReceiveOutcome::Frame(f) = r.accept(d).expect("приём") {
                frame = Some(f);
            }
        }
        let frame = frame.expect("данные целы — кадр обязан собраться");
        assert_eq!(frame.data, data);
        // Восстанавливать было нечего: счётчик обязан остаться нулевым,
        // иначе «FEC пригодился» не отличить от «FEC просто включён».
        assert_eq!(r.recovered_frames(), 0);
    }

    #[test]
    fn rejects_parity_larger_than_count() {
        // Недоверенный заголовок: паритетных объявлено больше, чем
        // фрагментов. Без проверки сборщик ждал бы кадр из одного
        // паритета вечно.
        let mut datagram = vec![0u8; HEADER_SIZE + 4];
        datagram[0] = crate::packet::PROTOCOL_VERSION;
        datagram[1] = 1; // Video
        datagram[2..4].copy_from_slice(&(9u16 << 8).to_be_bytes()); // parity = 9
        datagram[14..16].copy_from_slice(&4u16.to_be_bytes()); // count = 4

        let mut r = Reassembler::new(2);
        assert!(r.accept(&datagram).is_err());
    }

    #[test]
    fn stream_with_losses_meets_stage6_criterion() {
        // Критерий этапа 6: при 3 % потерь датаграмов доля потерянных
        // КАДРОВ должна быть ≤ 5 % (было 51 % без FEC — находка 28).
        //
        // # Почему кадры перекрываются, а не идут по одному
        //
        // Первая версия теста подавала кадры строго последовательно:
        // все датаграмы кадра N, потом все кадра N+1. Она проходила
        // и с перемежением паритета, и без него — то есть **не ловила
        // регресс, ради которого написана** (находка 32: проверка
        // воспроизводила не то состояние).
        //
        // В живом потоке кадры перекрываются: пока кадр N ждёт
        // потерянный фрагмент, кадр N+1 уже приходит. Собравшись
        // первым, он вытесняет N через `drop_older_than` — и если
        // паритет N ещё не дошёл, кадр гибнет за шаг до спасения.
        // Именно это дало 45 % потерь в живом прогоне при 5 % здесь.
        let f = Fragmenter::with_max_payload(1200);
        let mut r = Reassembler::new(3);
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut sent = 0u64;
        let mut got = 0u64;

        // Датаграмы предыдущего кадра, ещё не отданные приёмнику:
        // они домешиваются к следующему, создавая перекрытие.
        let mut carry: Vec<Vec<u8>> = Vec::new();

        for seq in 1..=200u64 {
            // Размеры как в жизни: в основном разностные кадры,
            // изредка крупный ключевой.
            let size = if seq % 20 == 0 { 60_000 } else { 3_000 };
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            sent += 1;

            let mut datagrams = Vec::new();
            f.fragment_with_fec(
                PayloadKind::Video,
                seq,
                seq % 20 == 0,
                0,
                &data,
                FecPolicy::Fixed(20),
                |d| {
                    datagrams.push(d.to_vec());
                    Ok(())
                },
            )
            .expect("нарезка");

            // Хвост кадра откладывается и приходит ПОСЛЕ начала
            // следующего — так ведёт себя канал с задержкой.
            let split_at = datagrams.len() / 2;
            let tail = datagrams.split_off(split_at);

            let mut batch = std::mem::take(&mut carry);
            batch.extend(datagrams);
            carry = tail;

            for d in &batch {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let roll = (rng >> 11) as f64 / (1u64 << 53) as f64;
                if roll < 0.03 {
                    continue;
                }
                if let Ok(ReceiveOutcome::Frame(_)) = r.accept(d) {
                    got += 1;
                }
            }
        }
        for d in &carry {
            if let Ok(ReceiveOutcome::Frame(_)) = r.accept(d) {
                got += 1;
            }
        }

        let lost_pct = (sent - got) as f64 / sent as f64 * 100.0;
        assert!(
            lost_pct <= 5.0,
            "критерий этапа 6: потери кадров ≤ 5 %, получено {lost_pct:.1} % \
             (восстановлено {}, выброшено при живом паритете {})",
            r.recovered_frames(),
            r.evicted_with_parity()
        );
        // Восстановления обязаны быть: ноль означал бы, что при 3 %
        // потерь ни один кадр не пострадал, — то есть тест не
        // воспроизводит то, ради чего написан (находка 26).
        assert!(
            r.recovered_frames() > 0,
            "при 3 % потерь FEC обязан был хоть раз пригодиться"
        );
    }

    #[test]
    fn parity_is_interleaved_not_appended() {
        // Паритет обязан приходить ВПЕРЕМЕЖКУ с данными.
        //
        // Проверяется порядок отправки, а не доля потерь: статистика
        // потерь на синтетическом потоке нечувствительна к порядку
        // (кадры, гибнущие от нехватки паритета, гибнут при любом
        // порядке), и тест на ней проходил бы даже с отключённым
        // перемежением. Это ровно находка 32 — проверка, которая
        // воспроизводит не то состояние.
        //
        // Живой прогон показал цену порядка прямо: паритет в конце
        // дал 45.7 % потерянных кадров и 499 вытеснений при живом
        // паритете, перемежённый — 2.87 % и 43.
        let f = Fragmenter::with_max_payload(100);
        let data: Vec<u8> = (0..2000u32).map(|i| i as u8).collect();

        let mut kinds = Vec::new();
        f.fragment_with_fec(
            PayloadKind::Video,
            1,
            false,
            0,
            &data,
            FecPolicy::Fixed(20),
            |d| {
                let (h, _) = FragmentHeader::parse(d).expect("разбор");
                kinds.push(h.is_parity());
                Ok(())
            },
        )
        .expect("нарезка");

        let parity_positions: Vec<usize> = kinds
            .iter()
            .enumerate()
            .filter(|(_, is_p)| **is_p)
            .map(|(i, _)| i)
            .collect();
        assert!(!parity_positions.is_empty(), "паритет обязан быть");

        // Первый паритетный обязан прийти в первой половине кадра.
        // Если он позже, кадр не успеет спастись до прихода
        // следующего — ровно тот дефект, что дал 45 % потерь.
        let first = parity_positions[0];
        assert!(
            first < kinds.len() / 2,
            "первый паритетный на позиции {first} из {} — слишком поздно",
            kinds.len()
        );

        // И последний фрагмент кадра — данные, а не паритет:
        // иначе весь паритет опять оказался бы в хвосте.
        assert!(
            !kinds.last().copied().unwrap_or(true) || parity_positions.len() == 1,
            "паритет не должен скапливаться в конце"
        );
    }
}
