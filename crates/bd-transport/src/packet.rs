//! Wire-формат пакета: заголовок фрагмента и его разбор.
//!
//! # Почему заглушка использует настоящий формат
//!
//! Транспорт на этапе 1 — заглушка в памяти (docs/roadmap.md, этап 1,
//! пункт 3), но формат пакета здесь уже настоящий. Причина простая:
//! если заглушка гоняет кадры целиком, то на этапе 3 фрагментация,
//! нумерация и сборка появятся все сразу, и отлаживать их придётся
//! поверх реальной сети, где вдобавок есть потери и переупорядочивание.
//! Дешевле отладить сборщик здесь, где потери мы вносим сами и
//! воспроизводимо.
//!
//! # Формат
//!
//! Заголовок 24 байта, порядок байтов сетевой (big-endian):
//!
//! ```text
//! смещение  размер  поле
//!    0        1     версия протокола
//!    1        1     вид полезной нагрузки
//!    2        2     флаги
//!    4        8     номер кадра
//!   12        2     индекс фрагмента
//!   14        2     всего фрагментов
//!   16        8     метка времени захвата, мкс
//! ```
//!
//! # Зачем метка времени в заголовке
//!
//! Без неё **задержку между двумя машинами измерить нечем**: отметка
//! `Captured` ставится на хосте, а клиент о ней не знает. В заглушке
//! это было незаметно — обе стороны там один процесс с общей эпохой,
//! и тайминги просто лежали рядом в памяти. Через сеть выяснилось,
//! что glass-to-glass не считается вовсе (§0.1, находка 40).
//!
//! Метка идёт в **каждом** фрагменте, а не только в первом. Восемь
//! байт при полезной нагрузке 1146 — это 0.7 %, и такой ценой формат
//! остаётся одинаковым для всех фрагментов. Разный разбор для первого
//! и остальных стоил бы дороже этих семи промилле — и не в байтах,
//! а в ошибках.
//!
//! Номер кадра — 64-битный намеренно: при 60 fps 32 бита переполнились
//! бы за 2.3 года непрерывной работы, а сессия необслуживаемого доступа
//! (этап 8) вполне может жить месяцами. Переполнение счётчика в
//! сборщике кадров — это не «редкий сбой», а тихая порча картинки.

use crate::error::{Result, TransportError};

/// Версия wire-формата.
///
/// Проверяется на приёме: пакет чужой версии отбрасывается, а не
/// разбирается «как получится».
pub const PROTOCOL_VERSION: u8 = 2;

/// Размер заголовка фрагмента в байтах.
pub const HEADER_SIZE: usize = 24;

/// Флаг: фрагмент принадлежит ключевому кадру.
const FLAG_KEYFRAME: u16 = 1 << 0;

/// Вид полезной нагрузки.
///
/// Транспорт не парсит содержимое (CLAUDE.md §4.2.4) — вид нужен
/// только для маршрутизации к нужному приёмнику и выбора приоритета.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadKind {
    /// Видеокадр. Unreliable: устаревший кадр не нужен.
    Video,
    /// Аудиопакет.
    Audio,
    /// Событие ввода.
    Input,
    /// Позиция курсора.
    ///
    /// # Почему у курсора свой вид, а не `Video`
    ///
    /// Пока курсор ехал отдельным соединением, вид в его заголовке
    /// не значил ничего: в том канале не было ничего другого, и
    /// позиция ходила под кодом `Video` просто потому, что код
    /// требовалось указать (находка 56).
    ///
    /// В сведённом соединении вид становится адресом. Позиция под
    /// кодом `Video` попала бы в сборку видеокадров, где своя
    /// нумерация, — и оба потока молча выбрасывали бы друг друга как
    /// устаревшие. Ровно тот дефект, что в находке 39, но внутри
    /// одной стороны.
    ///
    /// Отдельный вид, а не `Control`: форма курсора весит килобайты
    /// и меняется редко, позиция — девять байт сотни раз в секунду.
    /// Общая нумерация заставила бы позицию ждать формы.
    Cursor,
    /// Рукопожатие авторизации: пароль и ответ на него.
    ///
    /// Свой вид, а не `Control`: там едет форма курсора, у неё своя
    /// нумерация и свой темп. В общей сборке пароль и курсор
    /// выбрасывали бы пакеты друг друга как устаревшие — ровно
    /// дефект находки 60, но на пути, где цена ошибки выше.
    Auth,
    /// Управляющее сообщение.
    Control,
}

impl PayloadKind {
    /// Код вида в wire-формате.
    const fn code(self) -> u8 {
        match self {
            PayloadKind::Video => 1,
            PayloadKind::Audio => 2,
            PayloadKind::Input => 3,
            PayloadKind::Control => 4,
            // Курсор получил код 5, а не место рядом с видео:
            // существующие коды сдвигать нельзя, иначе стороны разных
            // сборок разобрали бы одни и те же байты по-разному.
            PayloadKind::Cursor => 5,
            PayloadKind::Auth => 6,
        }
    }

    /// Все виды — для проверок полноты кодирования.
    #[cfg(test)]
    const ALL: [Self; 6] = [
        Self::Video,
        Self::Audio,
        Self::Input,
        Self::Control,
        Self::Cursor,
        Self::Auth,
    ];

    /// Разобрать код вида.
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(PayloadKind::Video),
            2 => Some(PayloadKind::Audio),
            3 => Some(PayloadKind::Input),
            4 => Some(PayloadKind::Control),
            5 => Some(PayloadKind::Cursor),
            6 => Some(PayloadKind::Auth),
            _ => None,
        }
    }
}

/// Заголовок одного фрагмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    /// Вид полезной нагрузки.
    pub kind: PayloadKind,
    /// Номер кадра, к которому относится фрагмент.
    pub sequence: u64,
    /// Индекс фрагмента внутри кадра, от нуля.
    pub index: u16,
    /// Общее число фрагментов кадра.
    pub count: u16,
    /// Принадлежит ли фрагмент ключевому кадру.
    ///
    /// Нужно приёмнику: неполный ключевой кадр стоит подождать, а
    /// неполный разностный — выбросить сразу.
    pub keyframe: bool,
    /// Когда кадр был захвачен на отправителе, в микросекундах его
    /// эпохи.
    ///
    /// **Часы сторон не синхронизированы**, поэтому вычитать это
    /// значение из своего времени напрямую нельзя: получится разница
    /// часов, а не задержка. Смещение оценивает приёмник
    /// (см. `bd_core::time::ClockSync`).
    pub captured_at_micros: u64,
}

impl FragmentHeader {
    /// Записать заголовок в начало буфера.
    ///
    /// Буфер должен быть не короче [`HEADER_SIZE`].
    pub fn write_to(&self, out: &mut [u8]) -> Result<()> {
        if out.len() < HEADER_SIZE {
            return Err(TransportError::Malformed("буфер короче заголовка"));
        }
        let flags = if self.keyframe { FLAG_KEYFRAME } else { 0 };

        out[0] = PROTOCOL_VERSION;
        out[1] = self.kind.code();
        out[2..4].copy_from_slice(&flags.to_be_bytes());
        out[4..12].copy_from_slice(&self.sequence.to_be_bytes());
        out[12..14].copy_from_slice(&self.index.to_be_bytes());
        out[14..16].copy_from_slice(&self.count.to_be_bytes());
        out[16..24].copy_from_slice(&self.captured_at_micros.to_be_bytes());
        Ok(())
    }

    /// Разобрать заголовок и вернуть его вместе с полезной нагрузкой.
    ///
    /// # Недоверенные данные
    ///
    /// Это первая функция, встречающая байты из сети. Никакого
    /// `transmute` буфера в структуру (CLAUDE.md §4.3.5): только явное
    /// чтение полей с проверкой длины. Все инварианты проверяются
    /// здесь, чтобы сборщик кадров мог им доверять.
    pub fn parse(datagram: &[u8]) -> Result<(Self, &[u8])> {
        if datagram.len() < HEADER_SIZE {
            return Err(TransportError::Malformed("датаграм короче заголовка"));
        }

        if datagram[0] != PROTOCOL_VERSION {
            return Err(TransportError::Malformed("чужая версия протокола"));
        }

        let kind = PayloadKind::from_code(datagram[1])
            .ok_or(TransportError::Malformed("неизвестный вид нагрузки"))?;

        // Длины срезов уже гарантированы проверкой выше, но
        // `unwrap` в пути недоверенных данных запрещён (§4.3.6),
        // поэтому преобразование идёт через try_into с явной ошибкой.
        let flags = u16::from_be_bytes(
            datagram[2..4]
                .try_into()
                .map_err(|_| TransportError::Malformed("флаги"))?,
        );
        let sequence = u64::from_be_bytes(
            datagram[4..12]
                .try_into()
                .map_err(|_| TransportError::Malformed("номер кадра"))?,
        );
        let index = u16::from_be_bytes(
            datagram[12..14]
                .try_into()
                .map_err(|_| TransportError::Malformed("индекс фрагмента"))?,
        );
        let count = u16::from_be_bytes(
            datagram[14..16]
                .try_into()
                .map_err(|_| TransportError::Malformed("число фрагментов"))?,
        );
        let captured_at_micros = u64::from_be_bytes(
            datagram[16..24]
                .try_into()
                .map_err(|_| TransportError::Malformed("метка времени"))?,
        );

        // Инварианты, на которые полагается сборщик. Без этих двух
        // проверок пакет от постороннего заставил бы приёмник выделить
        // вектор на 65535 слотов и ждать фрагменты, которых нет.
        if count == 0 {
            return Err(TransportError::Malformed("нулевое число фрагментов"));
        }
        if index >= count {
            return Err(TransportError::Malformed("индекс вне диапазона"));
        }

        let header = Self {
            kind,
            sequence,
            index,
            count,
            keyframe: flags & FLAG_KEYFRAME != 0,
            captured_at_micros,
        };
        Ok((header, &datagram[HEADER_SIZE..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FragmentHeader {
        FragmentHeader {
            kind: PayloadKind::Video,
            sequence: 0x0102_0304_0506_0708,
            index: 3,
            count: 7,
            keyframe: true,
            // Значение с единицами во всех байтах: сдвиг или обрезка
            // при записи изменили бы его заметно, а круглое число
            // такую ошибку могло бы пережить.
            captured_at_micros: 0x1122_3344_5566_7788,
        }
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let mut buf = vec![0u8; HEADER_SIZE + 4];
        sample().write_to(&mut buf).expect("запись заголовка");
        buf[HEADER_SIZE..].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let (header, payload) = FragmentHeader::parse(&buf).expect("разбор");
        assert_eq!(header, sample());
        assert_eq!(payload, &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn empty_payload_is_valid() {
        // Кадр нулевой длины бессмыслен, но это забота уровня выше:
        // разбор заголовка обязан отработать без паники.
        let mut buf = vec![0u8; HEADER_SIZE];
        sample().write_to(&mut buf).expect("запись");
        let (_, payload) = FragmentHeader::parse(&buf).expect("разбор");
        assert!(payload.is_empty());
    }

    #[test]
    fn truncated_datagram_is_rejected() {
        let mut buf = vec![0u8; HEADER_SIZE];
        sample().write_to(&mut buf).expect("запись");
        // Каждая длина короче заголовка обязана дать ошибку, а не панику.
        for len in 0..HEADER_SIZE {
            assert!(
                FragmentHeader::parse(&buf[..len]).is_err(),
                "длина {len} должна быть отвергнута"
            );
        }
    }

    #[test]
    fn foreign_version_is_rejected() {
        let mut buf = vec![0u8; HEADER_SIZE];
        sample().write_to(&mut buf).expect("запись");
        buf[0] = PROTOCOL_VERSION.wrapping_add(1);
        assert!(FragmentHeader::parse(&buf).is_err());
    }

    #[test]
    fn every_kind_survives_the_wire() {
        // Вид нагрузки стал адресом: по нему приёмник выбирает, в
        // какую сборку класть фрагмент. Код, потерявшийся при
        // кодировании, не даст ошибки — просто один из потоков
        // перестанет доходить, и искать причину будут не здесь.
        for kind in PayloadKind::ALL {
            let mut buf = vec![0u8; HEADER_SIZE];
            FragmentHeader { kind, ..sample() }
                .write_to(&mut buf)
                .expect("запись");
            let (header, _) = FragmentHeader::parse(&buf).expect("разбор");
            assert_eq!(header.kind, kind, "вид {kind:?} не пережил путь");
        }
    }

    #[test]
    fn kind_codes_are_distinct() {
        // Два вида с одним кодом означали бы, что приёмник кладёт их
        // в одну сборку, — то есть ровно тот дефект, ради устранения
        // которого состояние сборщика разделено по видам.
        let mut codes: Vec<u8> = PayloadKind::ALL.iter().map(|k| k.code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "коды видов обязаны быть различны");
    }

    #[test]
    fn unknown_payload_kind_is_rejected() {
        let mut buf = vec![0u8; HEADER_SIZE];
        sample().write_to(&mut buf).expect("запись");
        buf[1] = 200;
        assert!(FragmentHeader::parse(&buf).is_err());
    }

    #[test]
    fn zero_count_is_rejected() {
        // Без этой проверки сборщик ждал бы кадр из нуля фрагментов вечно.
        let mut buf = vec![0u8; HEADER_SIZE];
        sample().write_to(&mut buf).expect("запись");
        buf[14..16].copy_from_slice(&0u16.to_be_bytes());
        assert!(FragmentHeader::parse(&buf).is_err());
    }

    #[test]
    fn index_beyond_count_is_rejected() {
        // Без этой проверки сборщик писал бы за границу слота.
        let mut buf = vec![0u8; HEADER_SIZE];
        sample().write_to(&mut buf).expect("запись");
        buf[12..14].copy_from_slice(&7u16.to_be_bytes()); // count == 7
        assert!(FragmentHeader::parse(&buf).is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Грубая замена фаззингу до появления цели cargo-fuzz (§10.3):
        // детерминированный перебор мусора. Требование одно — не паниковать.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (state >> (i % 8 * 8)) as u8).collect();
            let _ = FragmentHeader::parse(&bytes);
        }
    }
}
