//! Фаззинг парсеров недоверенных данных (§10.3, критерий этапа 5).
//!
//! # Что здесь проверяется
//!
//! Всё, что первым встречает байты от постороннего: события ввода,
//! курсор, рукопожатие авторизации. Эти парсеры вызываются **до**
//! всякой проверки прав, и паника в любом из них — это отказ в
//! обслуживании по одному датаграму от кого угодно.
//!
//! Проверяются три свойства, и порядок важности именно такой:
//!
//! 1. **не паникует ни на чём.** Главное. `Option::None` на мусор —
//!    правильный ответ, паника — уязвимость;
//! 2. **не принимает мусор за валидное.** Парсер, который «чинит»
//!    непонятный ввод, хуже отсутствующего: он применит не то, что
//!    прислали;
//! 3. **encode → parse возвращает то же самое.** Иначе формат теряет
//!    данные молча, а расхождение сторон выглядит как сетевой сбой.
//!
//! # Почему proptest, а не cargo-fuzz
//!
//! `cargo-fuzz` (libFuzzer) требует **nightly**, а `rust-toolchain.toml`
//! прибит к stable 1.98.0 с этапа 0. Менять это ради одного критерия
//! дороже, чем кажется: nightly ломается сам по себе, а сборка
//! продукта обязана быть воспроизводимой.
//!
//! **Честно о разнице.** libFuzzer ведёт поиск по покрытию: он видит,
//! какие ветки исполнились, и целит в неисследованные. `proptest`
//! генерирует входы вслепую по заданной форме. На парсере с
//! фиксированной длиной записи (наш случай — 9, 25, 9 байт) разница
//! невелика: пространство входов маленькое и перебирается плотно. На
//! парсере с вложенными структурами она была бы существенной.
//!
//! Поэтому критерий «фаззер отработал час» здесь выполнен **не
//! буквально**, и это записано в roadmap, а не замолчано.

use bd_core::auth::{AuthRequest, AuthResponse};
use bd_core::cursor::CursorPosition;
use bd_core::input::{InputEvent, SequencedInput};
use proptest::prelude::*;

/// Сколько случаев прогонять на каждое свойство.
///
/// 4096 против 256 по умолчанию: парсеры дешёвые (десятки
/// наносекунд), и весь файл всё равно отрабатывает за секунды. При
/// этом плотность покрытия короткого входа заметно выше.
const CASES: u32 = 4096;

/// Размер события ввода в wire-формате.
///
/// Константы в `bd-core` приватные — и правильно: снаружи их знать не
/// обязаны, а публичное число легко случайно «уточнить». Здесь они
/// выписаны заново намеренно: это **проверка формата снаружи**, и
/// разойдись она с реализацией, тесты roundtrip упадут сразу же.
const EVENT_SIZE: usize = 9;

/// Размер позиции курсора в wire-формате.
const POSITION_SIZE: usize = 9;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// Байты любой длины и содержания не должны ронять парсер.
    ///
    /// Это главное свойство: датаграм приходит от кого угодно, и
    /// паника здесь означала бы, что процесс роняется одним пакетом.
    #[test]
    fn input_event_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = InputEvent::parse(&bytes);
    }

    #[test]
    fn sequenced_input_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..80)) {
        let _ = SequencedInput::parse(&bytes);
    }

    #[test]
    fn cursor_position_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = CursorPosition::parse(&bytes);
    }

    /// Строки — отдельный случай: авторизация текстовая, и туда
    /// приходит всё, что человек или атакующий наберёт.
    #[test]
    fn auth_never_panics(text in ".*") {
        let _ = AuthRequest::parse(&text);
        let _ = AuthResponse::parse(&text);
    }

    /// UTF-8 из произвольных байт: строка может быть и не текстом.
    #[test]
    fn auth_never_panics_on_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let _ = AuthRequest::parse(text);
            let _ = AuthResponse::parse(text);
        }
    }

    /// **Мусор не должен разбираться в разрешение доступа.**
    ///
    /// Отдельное свойство, а не частный случай «не паникует»: паника
    /// заметна сразу, а вот сбой, превращающий случайную строку в
    /// `Granted`, тих и означает отсутствие защиты.
    #[test]
    fn garbage_never_grants_access(text in ".*") {
        if text != "granted" {
            prop_assert_ne!(AuthResponse::parse(&text), Some(AuthResponse::Granted));
        }
    }

    /// То же, но **вблизи** валидного слова.
    ///
    /// # Зачем отдельно, если свойство то же самое
    ///
    /// Потому что предыдущий тест его не ловит, и это выяснилось
    /// проверкой на способность провалиться: с подменённым разбором
    /// (`"granted" | "grante" => Granted`) он **прошёл**. Случайная
    /// строка почти никогда не оказывается похожей на `granted` —
    /// пространство слишком велико.
    ///
    /// Ровно та разница, о которой сказано в шапке: coverage-guided
    /// фаззер дошёл бы до соседней ветки сам, `proptest` генерирует
    /// вслепую. Лечится тем, что окрестность валидного входа задаётся
    /// явно — мутациями от него самого.
    #[test]
    fn near_miss_never_grants_access(
        cut in 0usize..8,
        extra in "[a-z]{0,3}",
        case_shift in any::<bool>(),
    ) {
        let base = "granted";

        // Обрезки: "grante", "grant", ... — то, во что превращается
        // слово при потере хвоста.
        let truncated = &base[..cut.min(base.len())];
        // Дописки: "grantedx", "grantedab".
        let extended = format!("{base}{extra}");
        // Смена регистра: разбор обязан быть точным.
        let shifted = if case_shift { base.to_uppercase() } else { base.to_string() };

        for candidate in [truncated.to_string(), extended, shifted] {
            if candidate != base {
                prop_assert_ne!(
                    AuthResponse::parse(&candidate),
                    Some(AuthResponse::Granted),
                    "строка {:?} разобралась как разрешение доступа",
                    candidate
                );
            }
        }
    }

    /// Событие ввода переживает кодирование и разбор.
    ///
    /// Сравнение с допуском формата: координаты едут в `u16`, то есть
    /// 0.25 возвращается как 0.24998856. Точное `==` объявило бы это
    /// искажением — ровно находка 35, где проба сообщила о «77
    /// искажённых событиях» при исправном транспорте.
    #[test]
    fn input_event_survives_roundtrip(
        x in 0.0f32..=1.0,
        y in 0.0f32..=1.0,
        code in any::<u16>(),
    ) {
        use bd_core::input::{KeyCode, MousePosition};

        for event in [
            InputEvent::MouseMove {
                position: MousePosition::new(x, y),
            },
            InputEvent::Key {
                key: KeyCode(code),
                pressed: true,
            },
            InputEvent::Key {
                key: KeyCode(code),
                pressed: false,
            },
        ] {
            let mut buffer = [0u8; EVENT_SIZE];
            event.encode(&mut buffer);

            // Собственное кодирование обязано разбираться. Если нет —
            // формат теряет данные молча, а расхождение сторон
            // выглядело бы как сетевой сбой.
            //
            // Исключение — неизвестный код клавиши: парсер отвергает
            // такие намеренно (применить не то, что прислали, хуже,
            // чем не применить ничего), и это правильное поведение.
            let Some(parsed) = InputEvent::parse(&buffer) else {
                prop_assert!(
                    matches!(event, InputEvent::Key { .. }),
                    "не разобралось собственное кодирование: {event:?}"
                );
                continue;
            };

            prop_assert!(
                event.eq_within_wire_precision(&parsed),
                "событие изменилось: {event:?} → {parsed:?}"
            );
        }
    }

    /// Позиция курсора переживает кодирование и разбор.
    #[test]
    fn cursor_position_survives_roundtrip(
        x in 0.0f32..=1.0,
        y in 0.0f32..=1.0,
        shape_id in any::<u32>(),
        visible in any::<bool>(),
    ) {
        let original = CursorPosition {
            position: bd_core::input::MousePosition::new(x, y),
            shape_id,
            visible,
        };
        let parsed = CursorPosition::parse(&original.encode())
            .expect("собственное кодирование обязано разбираться");

        prop_assert_eq!(parsed.shape_id, original.shape_id);
        prop_assert_eq!(parsed.visible, original.visible);
        // Координаты — с допуском формата (u16), как и у событий:
        // точное `==` объявило бы потерю точности искажением
        // (находка 35).
        prop_assert!((parsed.position.x() - original.position.x()).abs() < 1.0 / 65_535.0);
        prop_assert!((parsed.position.y() - original.position.y()).abs() < 1.0 / 65_535.0);
    }

    /// Вызов опознания переживает кодирование и разбор.
    ///
    /// Здесь допуска нет и быть не может: это криптографический вызов,
    /// и потеря даже одного бита сделала бы подпись непроверяемой.
    #[test]
    fn identify_survives_roundtrip(challenge in any::<[u8; 16]>()) {
        let original = AuthRequest::Identify { challenge };
        prop_assert_eq!(
            AuthRequest::parse(&original.encode()),
            Some(original)
        );
    }

    /// Ответ с ключом и подписью переживает кодирование и разбор.
    #[test]
    fn identity_survives_roundtrip(
        key in prop::collection::vec(any::<u8>(), 32..=32),
        signature in prop::collection::vec(any::<u8>(), 64..=64),
    ) {
        let original = AuthResponse::Identity {
            public_key: key,
            signature,
        };
        prop_assert_eq!(
            AuthResponse::parse(&original.encode()),
            Some(original)
        );
    }

    /// Пароль любой формы переживает кодирование — **кроме**
    /// содержащего разделитель.
    ///
    /// Табуляция внутри пароля не должна превращаться в лишнее поле:
    /// иначе подделанное сообщение разобралось бы как валидное.
    #[test]
    fn password_survives_roundtrip_unless_it_smuggles_a_separator(password in ".*") {
        let original = AuthRequest::Password(password.clone());
        let parsed = AuthRequest::parse(&original.encode());

        if password.contains('\t') {
            prop_assert_eq!(parsed, None, "разделитель в пароле обязан отвергаться");
        } else if original.encode().len() <= bd_core::auth::MAX_AUTH_LEN {
            prop_assert_eq!(parsed, Some(original));
        }
    }

    /// Разбор **никогда** не должен принимать вход неверной длины.
    ///
    /// Свойство важнее, чем кажется: приняв короткий буфер, парсер
    /// прочитал бы соседнюю память как часть события — а это уже
    /// не логическая ошибка, а чтение за границей смысла.
    #[test]
    fn wrong_length_is_always_rejected(
        bytes in prop::collection::vec(any::<u8>(), 0..64)
    ) {
        if bytes.len() != EVENT_SIZE {
            prop_assert_eq!(InputEvent::parse(&bytes), None);
        }
        if bytes.len() != POSITION_SIZE {
            prop_assert_eq!(CursorPosition::parse(&bytes), None);
        }
    }
}
