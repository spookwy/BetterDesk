//! Пиннинг ключей: «это та самая машина, к которой я подключался».
//!
//! # Дыра, которую это закрывает
//!
//! Записана в `bd_core::auth` с самого появления пароля: **сигналинг
//! недоверен** (§8.1). Он сводит стороны по девятизначному ID, и
//! ничто не мешает ему свести клиента с чужой машиной — или
//! атакующему занять чужой ID, пока хозяин не в сети.
//!
//! Пароль от этого не спасает, и это стоит проговорить, потому что
//! кажется наоборот. Пароль доказывает, что подключающийся знает
//! секрет. Он ничего не говорит о том, **кому этот секрет
//! сообщили**: человек продиктует шесть цифр подставному хосту с той
//! же готовностью, что настоящему, — он же не видит разницы.
//!
//! # Как это работает
//!
//! При первом подключении клиент запоминает публичный ключ хоста
//! (TOFU — trust on first use). При каждом следующем сличает. Ключ
//! сменился — громкое предупреждение, а не тихое переподключение.
//!
//! # Честно о пределах TOFU
//!
//! **Первое подключение не защищено.** Если атакующий подменил хоста
//! именно тогда, клиент запомнит ключ атакующего и будет считать его
//! законным. Это принципиальное свойство модели, а не недоделка:
//! ничто, кроме внешнего канала доверия, не отличает первую встречу
//! от подмены.
//!
//! Смягчается тем же, чем в SSH и Signal: отпечаток показывается
//! человеку, и его можно сверить голосом — тем же звонком, которым
//! всё равно диктуется пароль. Поэтому [`DevicePublicKey::fingerprint`]
//! и сделан читаемым вслух.
//!
//! Зато **все последующие** подключения защищены полностью: подмена
//! после первой встречи требует приватного ключа, а его у атакующего
//! нет.
//!
//! [`DevicePublicKey::fingerprint`]: crate::identity::DevicePublicKey::fingerprint

use std::collections::BTreeMap;

use crate::identity::{DevicePublicKey, IdentityError, PUBLIC_LEN};

/// Что делать по результату сличения ключа.
///
/// # Почему это не `bool` и не `Result`
///
/// Случаев три, и реакция на них **разная**: молча продолжить,
/// спросить человека, остановиться и кричать. `bool` заставил бы
/// вызывающего выбирать между «пустить» и «не пустить», потеряв
/// середину, а `Result` подтолкнул бы к `?` — то есть к выходу там,
/// где нужно спросить.
///
/// Это тот же довод, что в находке 38б: если ответ «нет» бывает по
/// разным причинам и реакция на них разная, тип обязан их различать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinVerdict {
    /// Ключ совпал с запомненным. Обычный случай, ничего не спрашиваем.
    Known,

    /// Машина видится впервые.
    ///
    /// Не ошибка: так выглядит всякое первое подключение. Но и не
    /// «всё в порядке» — человеку показывается отпечаток, чтобы он
    /// мог сверить его голосом с тем, кто сидит за хостом.
    FirstSight {
        /// Отпечаток для показа и сверки вслух.
        fingerprint: String,
    },

    /// **Ключ не тот, что был.** Либо подмена, либо хост переустановлен.
    ///
    /// Различить эти два случая изнутри невозможно, и притворяться,
    /// что можем, нельзя: решение принимает человек, а наше дело —
    /// дать ему обе строки отпечатков, чтобы он мог позвонить и
    /// спросить.
    Changed {
        /// Что было запомнено.
        expected: String,
        /// Что предъявлено сейчас.
        actual: String,
    },
}

impl PinVerdict {
    /// Можно ли продолжать без вопроса к человеку.
    ///
    /// Только [`Self::Known`]. И `FirstSight`, и `Changed` требуют
    /// решения человека — с разной, впрочем, срочностью.
    #[must_use]
    pub fn is_silent(&self) -> bool {
        matches!(self, Self::Known)
    }

    /// Требует ли это громкого предупреждения.
    ///
    /// Отделено от [`Self::is_silent`] намеренно: первое подключение
    /// и смена ключа **не равны по тревожности**, и показывать их
    /// одинаково — значит приучить человека нажимать «да».
    #[must_use]
    pub fn is_alarming(&self) -> bool {
        matches!(self, Self::Changed { .. })
    }
}

/// Список известных машин и их ключей.
///
/// # Почему `BTreeMap`, а не `HashMap`
///
/// Порядок обхода детерминирован, поэтому сериализация даёт
/// **одинаковые байты** при одинаковом содержимом. Это важно для
/// файла, который переписывается при каждой новой машине: иначе
/// diff между версиями был бы шумом, а сравнить две копии на разных
/// компьютерах стало бы нельзя.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PinnedKeys {
    /// ID машины → её публичный ключ.
    keys: BTreeMap<u32, DevicePublicKey>,
}

/// Ошибки разбора сохранённого списка.
#[derive(Debug, thiserror::Error)]
pub enum PinningError {
    /// Строка файла не разбирается.
    #[error("строка {line}: {reason}")]
    Malformed {
        /// Номер строки — чтобы человек нашёл её глазами.
        line: usize,
        /// Что именно не так.
        reason: &'static str,
    },

    /// Ключ разобран, но негоден.
    #[error("строка {line}: {source}")]
    BadKey {
        /// Номер строки.
        line: usize,
        /// Причина от разбора ключа.
        #[source]
        source: IdentityError,
    },
}

impl PinnedKeys {
    /// Пустой список — так выглядит первый запуск.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Сколько машин запомнено.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Пуст ли список.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Сличить предъявленный ключ с запомненным.
    ///
    /// **Ничего не меняет.** Запись — отдельным вызовом
    /// [`Self::remember`], и только после того, как человек согласился.
    /// Иначе «проверка» тихо узаконивала бы подменённый ключ — то
    /// есть делала бы ровно противоположное тому, ради чего написана.
    #[must_use]
    pub fn verdict(&self, device: u32, presented: &DevicePublicKey) -> PinVerdict {
        match self.keys.get(&device) {
            None => PinVerdict::FirstSight {
                fingerprint: presented.fingerprint(),
            },
            Some(known) if known == presented => PinVerdict::Known,
            Some(known) => PinVerdict::Changed {
                expected: known.fingerprint(),
                actual: presented.fingerprint(),
            },
        }
    }

    /// Запомнить ключ машины (или заменить прежний).
    ///
    /// Вызывать **после** согласия человека, а не вместо него.
    pub fn remember(&mut self, device: u32, key: DevicePublicKey) {
        self.keys.insert(device, key);
    }

    /// Забыть машину — например, когда человек сам сбросил доверие.
    ///
    /// Возвращает `true`, если она была в списке.
    pub fn forget(&mut self, device: u32) -> bool {
        self.keys.remove(&device).is_some()
    }

    /// Запомненный ключ машины, если есть.
    #[must_use]
    pub fn get(&self, device: u32) -> Option<&DevicePublicKey> {
        self.keys.get(&device)
    }

    /// Сохранить в текст.
    ///
    /// Формат — строка «ID пробел ключ-в-hex» на машину. Текстовый по
    /// той же причине, что и протокол авторизации: записей единицы,
    /// плотность не стоит ничего, а возможность открыть файл и
    /// глазами сверить отпечаток стоит дорого — особенно когда
    /// разбираются, почему выскочило предупреждение о подмене.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (device, key) in &self.keys {
            out.push_str(&format!("{device} {}\n", hex_encode(&key.to_bytes())));
        }
        out
    }

    /// Разобрать сохранённое.
    ///
    /// **Недоверенные данные**: файл мог испортиться или быть
    /// подменён локальным процессом. Ошибка называет номер строки —
    /// без него человек не найдёт, что чинить.
    ///
    /// Пустые строки пропускаются: они появляются от редактирования
    /// руками, и падать на них было бы недружелюбно.
    pub fn from_text(text: &str) -> Result<Self, PinningError> {
        let mut keys = BTreeMap::new();

        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }

            let mut parts = trimmed.split_whitespace();
            let device: u32 = parts
                .next()
                .ok_or(PinningError::Malformed {
                    line,
                    reason: "нет ID машины",
                })?
                .parse()
                .map_err(|_| PinningError::Malformed {
                    line,
                    reason: "ID машины не число",
                })?;

            let encoded = parts.next().ok_or(PinningError::Malformed {
                line,
                reason: "нет ключа",
            })?;

            if parts.next().is_some() {
                return Err(PinningError::Malformed {
                    line,
                    reason: "лишние поля",
                });
            }

            let bytes = hex_decode(encoded).ok_or(PinningError::Malformed {
                line,
                reason: "ключ не шестнадцатеричный",
            })?;

            let key = DevicePublicKey::from_bytes(&bytes)
                .map_err(|source| PinningError::BadKey { line, source })?;

            keys.insert(device, key);
        }

        Ok(Self { keys })
    }
}

fn hex_encode(bytes: &[u8; PUBLIC_LEN]) -> String {
    let mut out = String::with_capacity(PUBLIC_LEN * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() != PUBLIC_LEN * 2 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceIdentity;

    fn some_key() -> DevicePublicKey {
        DeviceIdentity::generate().expect("генерация").public()
    }

    #[test]
    fn first_sight_is_reported_not_silent() {
        let pins = PinnedKeys::new();
        let verdict = pins.verdict(123_456_789, &some_key());

        assert!(!verdict.is_silent(), "первая встреча требует показа");
        assert!(!verdict.is_alarming(), "но это не тревога");
        assert!(matches!(verdict, PinVerdict::FirstSight { .. }));
    }

    #[test]
    fn known_key_passes_silently() {
        let key = some_key();
        let mut pins = PinnedKeys::new();
        pins.remember(123_456_789, key);

        assert_eq!(pins.verdict(123_456_789, &key), PinVerdict::Known);
        assert!(pins.verdict(123_456_789, &key).is_silent());
    }

    /// **Главный тест модуля.** Ради этого случая он и написан:
    /// подставной хост под чужим ID обязан быть замечен.
    #[test]
    fn changed_key_is_alarming() {
        let genuine = some_key();
        let impostor = some_key();

        let mut pins = PinnedKeys::new();
        pins.remember(123_456_789, genuine);

        let verdict = pins.verdict(123_456_789, &impostor);
        assert!(verdict.is_alarming(), "подмена ключа обязана тревожить");

        let PinVerdict::Changed { expected, actual } = verdict else {
            panic!("ожидалась смена ключа");
        };
        // Обе строки нужны человеку: он сверяет их голосом.
        assert_eq!(expected, genuine.fingerprint());
        assert_eq!(actual, impostor.fingerprint());
        assert_ne!(expected, actual);
    }

    /// Проверка не должна ничего запоминать сама — иначе она
    /// узаконивала бы подменённый ключ при первом же сличении.
    #[test]
    fn verdict_does_not_remember_anything() {
        let pins = PinnedKeys::new();
        let _ = pins.verdict(123_456_789, &some_key());

        assert!(pins.is_empty(), "verdict обязан быть только чтением");
    }

    /// Разные машины не путаются между собой.
    #[test]
    fn keys_are_per_device() {
        let first = some_key();
        let second = some_key();

        let mut pins = PinnedKeys::new();
        pins.remember(111_111_111, first);
        pins.remember(222_222_222, second);

        assert_eq!(pins.verdict(111_111_111, &first), PinVerdict::Known);
        assert_eq!(pins.verdict(222_222_222, &second), PinVerdict::Known);
        // Ключ одной машины не годится для другой.
        assert!(pins.verdict(111_111_111, &second).is_alarming());
    }

    #[test]
    fn forgetting_returns_to_first_sight() {
        let key = some_key();
        let mut pins = PinnedKeys::new();
        pins.remember(123_456_789, key);

        assert!(pins.forget(123_456_789));
        assert!(!pins.forget(123_456_789), "повторное забывание — уже нет");
        assert!(matches!(
            pins.verdict(123_456_789, &key),
            PinVerdict::FirstSight { .. }
        ));
    }

    #[test]
    fn text_roundtrip_preserves_everything() {
        let mut pins = PinnedKeys::new();
        pins.remember(111_111_111, some_key());
        pins.remember(999_999_999, some_key());

        let restored = PinnedKeys::from_text(&pins.to_text()).expect("разбор");
        assert_eq!(pins, restored);
    }

    /// Сохранение обязано быть побайтово одинаковым при одинаковом
    /// содержимом — на этом основана возможность сверить две копии.
    #[test]
    fn text_output_is_deterministic() {
        let one = some_key();
        let two = some_key();

        let mut a = PinnedKeys::new();
        a.remember(999_999_999, two);
        a.remember(111_111_111, one);

        let mut b = PinnedKeys::new();
        b.remember(111_111_111, one);
        b.remember(999_999_999, two);

        assert_eq!(a.to_text(), b.to_text());
    }

    #[test]
    fn empty_text_gives_empty_list() {
        assert!(PinnedKeys::from_text("").expect("разбор").is_empty());
        assert!(PinnedKeys::from_text("\n\n  \n")
            .expect("разбор")
            .is_empty());
    }

    /// Недоверенный ввод: каждая поломка называет строку.
    #[test]
    fn malformed_lines_are_rejected_with_line_number() {
        let cases = [
            "123456789",            // нет ключа
            "ne-chislo 00",         // ID не число
            "123456789 zzzz",       // не hex
            "123456789 00 lishnee", // лишние поля
        ];

        for case in cases {
            assert!(
                PinnedKeys::from_text(case).is_err(),
                "должно быть отвергнуто: {case}"
            );
        }
    }

    /// Слабый ключ не проходит и через файл — иначе проверку
    /// [`DevicePublicKey::from_bytes`] можно было бы обойти,
    /// подложив его в список доверенных.
    #[test]
    fn weak_key_in_file_is_rejected() {
        let low_order = "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f";
        let text = format!("123456789 {low_order}");

        assert!(PinnedKeys::from_text(&text).is_err());
    }

    /// Ошибка разбора называет номер строки: без него человек не
    /// найдёт, что чинить, в файле на сотню машин.
    #[test]
    fn error_names_the_line() {
        let text = "111111111 00\n";
        let error = PinnedKeys::from_text(text).expect_err("должно упасть");

        assert!(error.to_string().contains("строка 1"), "{error}");
    }
}
