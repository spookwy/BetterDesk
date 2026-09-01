//! Рукопожатие авторизации: клиент доказывает знание пароля.
//!
//! # Место в модели доступа
//!
//! ID — удобное имя, а не секрет (§7.3): он короткий, печатается на
//! экране и диктуется вслух. Пароль — то, что человек за хостом
//! сообщает сам, и без него подключение отклоняется.
//!
//! # Чего эти сообщения НЕ дают
//!
//! Защиты от подмены хоста. Сигналинг недоверен (§8.1), и клиент
//! сейчас не может проверить, с тем ли он соединился: подставной хост
//! примет любой пароль и покажет свой экран. Лечится пиннингом ключей
//! устройств (этап 5) — до тех пор это остаётся дырой, и записать её
//! важнее, чем закрыть наполовину.
//!
//! Второе: пароль едет **внутри QUIC**, то есть зашифрован TLS 1.3.
//! Но сертификат хоста самоподписанный и не проверяется (§7.3), так
//! что перехват возможен при активной подмене. Против пассивного
//! наблюдателя защита есть, против активного — нет.
//!
//! # Почему формат текстовый
//!
//! Сообщений два за сессию. Плотность здесь не стоит ничего, а
//! читаемость стоит дорого: это первое, что отлаживают между двумя
//! машинами через интернет.

use core::fmt;

/// Предел длины сообщения авторизации.
///
/// Недоверенный ввод: приходит от кого угодно до всякой проверки
/// (§8.5). Без предела строка от постороннего заставила бы хост
/// выделять память по чужой команде.
pub const MAX_AUTH_LEN: usize = 256;

/// Сколько попыток пароля даётся клиенту.
///
/// Три — как у банковской карты. Пароль из шести цифр перебирается
/// за миллион попыток, и без предела соединение стало бы удобным
/// каналом для перебора: QUIC держит сессию, а хост отвечал бы
/// быстро.
///
/// После исчерпания сессия рвётся, и клиенту приходится соединяться
/// заново — а хост при каждом запуске выдаёт новый пароль.
pub const MAX_ATTEMPTS: u8 = 3;

/// Длина вызова в байтах.
///
/// # Зачем вызов вообще нужен
///
/// Без него хост мог бы прислать подпись, записанную заранее, — и
/// её повторил бы кто угодно, кто её однажды подслушал (replay).
/// Случайный вызов делает подпись годной ровно для одной сессии:
/// подписывается то, чего атакующий не мог видеть.
///
/// Шестнадцать байт — 128 бит: повтор вызова не встретится за время
/// жизни продукта, а сообщение остаётся коротким.
pub const CHALLENGE_LEN: usize = 16;

/// Длина публичного ключа Ed25519 в байтах.
///
/// Объявлена здесь, а не берётся из `bd-crypto`: `bd-core` не зависит
/// от криптографии (§4.2.1) и описывает только то, что едет по
/// проводу. Расхождение поймал бы тест `wire_sizes_match_ed25519`
/// в `bd-crypto` — там, где обе величины видны сразу (находка 62 про
/// разъехавшиеся копии).
pub const PUBLIC_KEY_LEN: usize = 32;

/// Длина подписи Ed25519 в байтах.
pub const SIGNATURE_LEN: usize = 64;

/// Сообщение авторизации от клиента к хосту.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRequest {
    /// «Назовись»: клиент шлёт случайный вызов, хост обязан подписать
    /// его своим ключом устройства.
    ///
    /// Идёт **первым**, до пароля, и порядок здесь принципиален.
    /// Пароль — секрет человека; отдавать его раньше, чем выяснено,
    /// с кем разговариваем, значит сообщать его кому попало. Именно
    /// в этом и состояла дыра: подставной хост принимал любой пароль.
    Identify {
        /// Случайные байты, которые хост подпишет.
        challenge: [u8; CHALLENGE_LEN],
    },

    /// «Вот пароль, пусти меня».
    Password(String),
}

/// Ответ хоста.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResponse {
    /// Пароль верный, можно работать.
    Granted,
    /// Пароль неверный; сколько попыток осталось.
    ///
    /// Число попыток сообщается намеренно: человек, ошибшийся при
    /// наборе, должен понимать, сколько у него ещё есть. Атакующему
    /// это ничего не даёт — он и так знает, что попыток конечное
    /// число.
    Denied {
        /// Сколько попыток осталось; 0 — сессия сейчас оборвётся.
        attempts_left: u8,
    },
    /// Человек за хостом отказал в подключении.
    ///
    /// Отдельно от `Denied`: пароль мог быть верным, но хозяин машины
    /// нажал «отклонить». Смешивать их нельзя — человек на клиенте
    /// иначе будет заново вводить правильный пароль.
    Rejected,

    /// Ответ на [`AuthRequest::Identify`]: ключ устройства и подпись
    /// присланного вызова.
    ///
    /// Проверяется в `bd-crypto`, а не здесь: `bd-core` не зависит от
    /// криптографии (§4.2.1), и это разделение полезно само по себе —
    /// протокол описывает, что едет по проводу, а не кому верить.
    Identity {
        /// Публичный ключ устройства, 32 байта в hex.
        public_key: Vec<u8>,
        /// Подпись вызова этим ключом, 64 байта в hex.
        signature: Vec<u8>,
    },
}

/// Разделитель полей. Табуляция: в пароле её быть не может, а пробел
/// теоретически может.
const SEP: char = '\t';

/// Перевести байты в шестнадцатеричную строку.
///
/// Протокол текстовый (см. шапку модуля), а ключи и подписи —
/// двоичные. Hex, а не base64: он читается глазами в дампе и
/// сверяется с отпечатком, который видит человек, — а отладка между
/// двумя машинами и есть то, ради чего формат текстовый.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Разобрать шестнадцатеричную строку заданной длины.
///
/// **Недоверенный ввод.** Длина проверяется до разбора: без этого
/// посторонний прислал бы строку любого размера, и мы выделяли бы
/// память по чужой команде (§8.5).
fn from_hex(text: &str, expected_bytes: usize) -> Option<Vec<u8>> {
    if text.len() != expected_bytes * 2 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

impl AuthRequest {
    /// Закодировать для отправки.
    pub fn encode(&self) -> String {
        match self {
            Self::Identify { challenge } => format!("identify{SEP}{}", to_hex(challenge)),
            Self::Password(p) => format!("auth{SEP}{p}"),
        }
    }

    /// Разобрать пришедшее от клиента.
    ///
    /// `None` на всё непонятное. Это недоверенные данные: единственно
    /// верная реакция — отбросить, а не угадывать намерение (§4.3.5).
    pub fn parse(input: &str) -> Option<Self> {
        if input.len() > MAX_AUTH_LEN {
            return None;
        }
        let mut parts = input.split(SEP);
        match parts.next()? {
            "identify" => {
                let challenge = from_hex(parts.next()?, CHALLENGE_LEN)?;
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::Identify {
                    challenge: challenge.try_into().ok()?,
                })
            }
            "auth" => {
                let password = parts.next()?;
                // Лишние поля — другая версия протокола или подделка.
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::Password(password.to_string()))
            }
            _ => None,
        }
    }
}

impl AuthResponse {
    /// Закодировать для отправки.
    pub fn encode(&self) -> String {
        match self {
            Self::Granted => "granted".to_string(),
            Self::Denied { attempts_left } => format!("denied{SEP}{attempts_left}"),
            Self::Rejected => "rejected".to_string(),
            Self::Identity {
                public_key,
                signature,
            } => format!(
                "identity{SEP}{}{SEP}{}",
                to_hex(public_key),
                to_hex(signature)
            ),
        }
    }

    /// Разобрать ответ хоста.
    pub fn parse(input: &str) -> Option<Self> {
        if input.len() > MAX_AUTH_LEN {
            return None;
        }
        let mut parts = input.split(SEP);
        let result = match parts.next()? {
            "granted" => Self::Granted,
            "rejected" => Self::Rejected,
            "denied" => {
                let attempts_left = parts.next()?.parse().ok()?;
                Self::Denied { attempts_left }
            }
            "identity" => {
                let public_key = from_hex(parts.next()?, PUBLIC_KEY_LEN)?;
                let signature = from_hex(parts.next()?, SIGNATURE_LEN)?;
                Self::Identity {
                    public_key,
                    signature,
                }
            }
            _ => return None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(result)
    }
}

impl fmt::Display for AuthResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Granted => write!(f, "доступ разрешён"),
            Self::Denied { attempts_left: 0 } => write!(f, "неверный пароль, попытки кончились"),
            Self::Denied { attempts_left } => {
                write!(f, "неверный пароль, осталось попыток: {attempts_left}")
            }
            Self::Rejected => write!(f, "хозяин машины отклонил подключение"),
            Self::Identity { .. } => write!(f, "хост назвал свой ключ"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let original = AuthRequest::Password("123456".into());
        let parsed = AuthRequest::parse(&original.encode()).expect("разбор");
        assert_eq!(parsed, original);
    }

    #[test]
    fn response_roundtrip() {
        for original in [
            AuthResponse::Granted,
            AuthResponse::Rejected,
            AuthResponse::Denied { attempts_left: 2 },
            AuthResponse::Denied { attempts_left: 0 },
        ] {
            let parsed = AuthResponse::parse(&original.encode()).expect("разбор");
            assert_eq!(parsed, original, "не пережило кодирование: {original:?}");
        }
    }

    #[test]
    fn identify_roundtrip() {
        let original = AuthRequest::Identify {
            challenge: [7u8; CHALLENGE_LEN],
        };
        assert_eq!(AuthRequest::parse(&original.encode()), Some(original));
    }

    #[test]
    fn identity_roundtrip() {
        let original = AuthResponse::Identity {
            public_key: vec![0xABu8; PUBLIC_KEY_LEN],
            signature: vec![0xCDu8; SIGNATURE_LEN],
        };
        assert_eq!(AuthResponse::parse(&original.encode()), Some(original));
    }

    /// Сообщение с ключом и подписью обязано влезать в предел длины —
    /// иначе `parse` отвергал бы **собственное** корректное сообщение,
    /// и опознание не работало бы вовсе, а причина выглядела бы как
    /// сетевая.
    #[test]
    fn identity_message_fits_the_limit() {
        let encoded = AuthResponse::Identity {
            public_key: vec![0u8; PUBLIC_KEY_LEN],
            signature: vec![0u8; SIGNATURE_LEN],
        }
        .encode();

        assert!(
            encoded.len() <= MAX_AUTH_LEN,
            "сообщение {} байт при пределе {MAX_AUTH_LEN}",
            encoded.len()
        );
    }

    /// Недоверенный ввод: ключ или подпись неверной длины отвергаются.
    /// Без этой проверки короткая подпись дошла бы до криптографии,
    /// где вызвала бы отказ с невнятной причиной.
    #[test]
    fn identity_of_wrong_length_is_rejected() {
        let short_key = format!("identity\t{}\t{}", "ab".repeat(31), "cd".repeat(64));
        let short_sig = format!("identity\t{}\t{}", "ab".repeat(32), "cd".repeat(63));
        let not_hex = format!("identity\t{}\t{}", "zz".repeat(32), "cd".repeat(64));

        assert!(AuthResponse::parse(&short_key).is_none());
        assert!(AuthResponse::parse(&short_sig).is_none());
        assert!(AuthResponse::parse(&not_hex).is_none());
    }

    #[test]
    fn challenge_of_wrong_length_is_rejected() {
        assert!(AuthRequest::parse(&format!("identify\t{}", "ab".repeat(15))).is_none());
        assert!(AuthRequest::parse(&format!("identify\t{}", "ab".repeat(17))).is_none());
        assert!(AuthRequest::parse("identify\t").is_none());
    }

    #[test]
    fn oversized_input_is_rejected() {
        // Без предела длины посторонний заставил бы нас выделять
        // память по своей команде (§8.5).
        let huge = format!("auth\t{}", "9".repeat(MAX_AUTH_LEN));
        assert!(AuthRequest::parse(&huge).is_none());
        assert!(AuthResponse::parse(&"granted".repeat(100)).is_none());
    }

    #[test]
    fn garbage_is_rejected_not_guessed() {
        for junk in [
            "",
            "auth",
            "granted\textra",
            "denied",
            "denied\tабв",
            "хрень",
        ] {
            let _ = AuthRequest::parse(junk);
            let _ = AuthResponse::parse(junk);
        }
        // Главное — ни один мусор не должен разобраться в «доступ
        // разрешён»: это превратило бы сбой в отсутствие защиты.
        for junk in ["", "auth", "granted\textra", "denied", "хрень", "grante"] {
            assert_ne!(
                AuthResponse::parse(junk),
                Some(AuthResponse::Granted),
                "мусор {junk:?} разобрался как разрешение"
            );
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Замена фаззингу до появления цели cargo-fuzz (§10.3).
        let mut state = 0x243F_6A88_85A3_08D3u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 300) as usize;
            let s: String = (0..len)
                .map(|i| char::from((state >> (i % 8 * 8)) as u8 | 1))
                .collect();
            let _ = AuthRequest::parse(&s);
            let _ = AuthResponse::parse(&s);
        }
    }

    #[test]
    fn password_with_separator_does_not_smuggle_fields() {
        // Пароль приходит от человека и может содержать что угодно.
        // Табуляция внутри него не должна превращаться в лишнее поле —
        // иначе разбор принял бы подделку за валидное сообщение.
        let sneaky = AuthRequest::Password("123\t456".into());
        assert!(
            AuthRequest::parse(&sneaky.encode()).is_none(),
            "пароль с табуляцией обязан быть отвергнут, а не разобран частично"
        );
    }
}
