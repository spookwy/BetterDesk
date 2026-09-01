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

/// Сообщение авторизации от клиента к хосту.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRequest {
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
}

/// Разделитель полей. Табуляция: в пароле её быть не может, а пробел
/// теоретически может.
const SEP: char = '\t';

impl AuthRequest {
    /// Закодировать для отправки.
    pub fn encode(&self) -> String {
        match self {
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
