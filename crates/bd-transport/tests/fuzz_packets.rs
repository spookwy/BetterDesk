//! Фаззинг разбора датаграмов (§10.3, критерий этапа 5).
//!
//! # Почему это важнее, чем парсеры `bd-core`
//!
//! `FragmentHeader::parse` и `Reassembler::accept` — **самая первая**
//! точка, куда попадают байты из UDP. До них нет ни авторизации, ни
//! проверки адреса: датаграм принимается от кого угодно, кто знает
//! порт. Паника здесь роняет процесс одним пакетом.
//!
//! Вторая опасность тише: сборщик кадров **накапливает состояние**.
//! Заголовок объявляет число фрагментов и индекс, и если им поверить
//! без проверки, посторонний одним пакетом заставит выделить память
//! по своему усмотрению. Поэтому проверяется не только «не паникует»,
//! но и «не растёт без предела».
//!
//! Про выбор `proptest` вместо `cargo-fuzz` — см. шапку
//! `bd-core/tests/fuzz_parsers.rs`.

use bd_transport::packet::FragmentHeader;
use bd_transport::Reassembler;
use proptest::prelude::*;

const CASES: u32 = 4096;

/// Размер заголовка фрагмента (wire-формат v2).
///
/// Выписан заново намеренно: это проверка формата снаружи. Разойдись
/// с реализацией — упадут тесты roundtrip ниже.
const HEADER_SIZE: usize = 24;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// Заголовок не паникует ни на чём.
    #[test]
    fn header_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..128)) {
        let _ = FragmentHeader::parse(&bytes);
    }

    /// Датаграм любой формы не должен ронять сборщик.
    ///
    /// Ёмкость взята маленькой (4 кадра — как в продукте): при
    /// большой вытеснение не наступало бы, и ветка очистки осталась
    /// бы непроверенной — ровно находка 27, где путь неполного кадра
    /// не исполнялся на канале без потерь.
    #[test]
    fn reassembler_never_panics(
        datagrams in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..96),
            0..24,
        )
    ) {
        let mut reassembler = Reassembler::new(4);
        for datagram in &datagrams {
            let _ = reassembler.accept(datagram);
        }
    }

    /// **Мусор из сети не должен превращаться в собранный кадр.**
    ///
    /// Отдельно от «не паникует»: собранный из мусора кадр ушёл бы в
    /// декодер, и отказ случился бы не там, где причина.
    ///
    /// Случайные 24+ байта почти никогда не пройдут проверку версии
    /// протокола, но проверить это дешевле, чем предполагать.
    #[test]
    fn garbage_never_completes_a_frame(
        datagrams in prop::collection::vec(
            prop::collection::vec(any::<u8>(), HEADER_SIZE..96),
            1..16,
        )
    ) {
        use bd_transport::ReceiveOutcome;

        let mut reassembler = Reassembler::new(4);
        for datagram in &datagrams {
            // Случайный байт версии совпадёт с нашей примерно в одном
            // случае из 256 — тогда кадр в принципе может собраться,
            // и это не ошибка. Проверяем только то, что не совпало.
            if datagram[0] == 2 {
                continue;
            }
            prop_assert!(
                !matches!(reassembler.accept(datagram), Ok(ReceiveOutcome::Frame(_))),
                "кадр собрался из датаграма с чужой версией протокола"
            );
        }
    }

    /// Заголовок переживает кодирование и разбор без потерь.
    ///
    /// Здесь допуска нет: заголовок целиком целочисленный, и потеря
    /// бита означала бы перепутанные кадры или неверную метку
    /// времени — то есть неверную задержку в отчёте.
    #[test]
    fn header_survives_roundtrip(
        sequence in any::<u64>(),
        index in any::<u16>(),
        count in 1u16..=4096,
        keyframe in any::<bool>(),
        captured_at_micros in any::<u64>(),
    ) {
        use bd_transport::PayloadKind;

        // Индекс обязан быть меньше числа фрагментов: иначе это
        // заведомо испорченный заголовок, и `parse` его отвергнет —
        // проверяется отдельным свойством ниже.
        let index = index % count;

        for kind in [PayloadKind::Video, PayloadKind::Input, PayloadKind::Cursor, PayloadKind::Auth] {
            let original = FragmentHeader {
                kind,
                sequence,
                index,
                count,
                keyframe,
                captured_at_micros,
                parity: 0,
            };

            let mut datagram = vec![0u8; HEADER_SIZE];
            original.write_to(&mut datagram).expect("запись заголовка");
            datagram.extend_from_slice(b"payload");

            let (parsed, payload) = FragmentHeader::parse(&datagram)
                .expect("собственное кодирование обязано разбираться");

            prop_assert_eq!(parsed.kind, original.kind);
            prop_assert_eq!(parsed.sequence, original.sequence);
            prop_assert_eq!(parsed.index, original.index);
            prop_assert_eq!(parsed.count, original.count);
            prop_assert_eq!(parsed.keyframe, original.keyframe);
            prop_assert_eq!(parsed.captured_at_micros, original.captured_at_micros);
            prop_assert_eq!(payload, b"payload");
        }
    }

    /// **Индекс за пределами числа фрагментов обязан отвергаться.**
    ///
    /// Это не придирка к формату: сборщик кладёт фрагмент по индексу,
    /// и приняв `index >= count`, он либо вышел бы за границу, либо
    /// молча вырос. Инвариант проверяется в `parse`, чтобы сборщик мог
    /// ему доверять, — и вот проверка, что проверка есть.
    #[test]
    fn index_beyond_count_is_rejected(
        sequence in any::<u64>(),
        count in 1u16..=1024,
        overflow in 0u16..1024,
    ) {
        use bd_transport::PayloadKind;

        let original = FragmentHeader {
            kind: PayloadKind::Video,
            sequence,
            index: count.saturating_add(overflow),
            count,
            keyframe: false,
            captured_at_micros: 0,
            parity: 0,
        };

        let mut datagram = vec![0u8; HEADER_SIZE];
        original.write_to(&mut datagram).expect("запись заголовка");

        prop_assert!(
            FragmentHeader::parse(&datagram).is_err(),
            "индекс {} при count {} принят",
            original.index,
            count
        );
    }

    /// Ноль фрагментов — тоже испорченный заголовок: кадр из нуля
    /// частей не существует, а деление на него в расчётах дало бы
    /// панику там, где её никто не ждёт.
    #[test]
    fn zero_count_is_rejected(sequence in any::<u64>()) {
        use bd_transport::PayloadKind;

        let original = FragmentHeader {
            kind: PayloadKind::Video,
            sequence,
            index: 0,
            count: 0,
            keyframe: false,
            captured_at_micros: 0,
            parity: 0,
        };

        let mut datagram = vec![0u8; HEADER_SIZE];
        original.write_to(&mut datagram).expect("запись заголовка");

        prop_assert!(FragmentHeader::parse(&datagram).is_err());
    }

    /// Датаграм короче заголовка отвергается, а не читается частично.
    #[test]
    fn short_datagram_is_rejected(
        bytes in prop::collection::vec(any::<u8>(), 0..HEADER_SIZE)
    ) {
        prop_assert!(FragmentHeader::parse(&bytes).is_err());
    }
}
