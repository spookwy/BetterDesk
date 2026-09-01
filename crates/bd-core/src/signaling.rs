//! Протокол сигналинга: как хост объявляется и как стороны находят
//! друг друга.
//!
//! # Что сигналинг делает и чего не делает
//!
//! Делает ровно две вещи: помнит, какой ID у какого адреса, и
//! передаёт одной стороне адрес другой. Через него **не идёт ни
//! видео, ни звук, ни ввод** — только несколько сообщений при
//! установлении связи (CLAUDE.md §3.1).
//!
//! Следствие для приватности: сервер видит, кто с кем и когда
//! соединяется, но не содержимое сессии. С этапа 5 он не сможет
//! и подменить сторону — ключи устройств проверяются напрямую.
//!
//! # Почему формат текстовый, а не бинарный
//!
//! Wire-формат видео двоичный и плотный, потому что идёт 60 раз
//! в секунду и каждый байт умножается на 3600. Здесь сообщений
//! **единицы за сессию**, и цена читаемости — ноль.
//!
//! Читаемость же стоит дорого: сигналинг единственная часть, которую
//! отлаживают между двумя машинами через интернет, где нет ни
//! отладчика, ни симметричного доступа. Возможность посмотреть
//! глазами, что улетело и что пришло, здесь важнее компактности.
//!
//! `bd-core` намеренно не зависит от `serde` (§4.2.1 — крейт обязан
//! собираться везде и не тащить лишнего), поэтому кодирование и
//! разбор написаны вручную. Формат от этого держится проще: плоские
//! пары `ключ=значение`, никакой вложенности.

use crate::device::DeviceId;
use core::fmt;

/// Максимальная длина сообщения сигналинга.
///
/// Недоверенный ввод: сообщение приходит из сети до всякой
/// аутентификации (§8.5). Без предела строка от постороннего
/// заставила бы сервер выделять память по чужой команде.
///
/// 512 байт — с запасом: самое длинное сообщение несёт ID, адрес
/// IPv6 и признак. Реально это меньше сотни байт.
pub const MAX_MESSAGE_LEN: usize = 512;

/// Адрес сигналинга по умолчанию.
///
/// # Зачем адрес вшит в программу
///
/// §7.1 требует «скачал и запустил»: человек, которому дали `.exe`,
/// не должен знать ни про какие серверы и уж тем более вводить их
/// адреса. Без вшитого значения он обязан был бы дописывать
/// `--signaling ws://...` — то есть настройку, которой у AnyDesk нет
/// и быть не должно.
///
/// # Почему без сервера всё же нельзя
///
/// Соблазн «пусть один из двух компьютеров и будет сервером» разбивается
/// о NAT. Компьютер за роутером **не знает своего внешнего адреса** —
/// он видит только `192.168.x.x`, и узнать настоящий можно
/// единственным способом: спросить кого-то снаружи. Плюс адрес одной
/// стороны надо передать другой ДО того, как между ними появилась
/// связь, — курица и яйцо.
///
/// Это и есть весь смысл сервера. Через него **не идёт ни видео, ни
/// звук, ни ввод** (§3.1): пара сообщений на установление сессии,
/// байты. Поэтому он бесплатен в эксплуатации и не становится узким
/// местом.
///
/// # Что делать, если он недоступен
///
/// Работать дальше. Подключение по адресу (`--connect`) сервера не
/// требует вовсе, и в локальной сети это полный, а не урезанный
/// режим. Значение переопределяется флагом `--signaling`: он остаётся
/// для своего сервера и для отладки.
pub const DEFAULT_SIGNALING: &str = "ws://89.168.99.202:9000/ws";

/// Сообщение от устройства к серверу.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// «Я хост с таким ID, мой адрес такой, я готов принимать».
    ///
    /// Адрес — тот, который хост видит у себя. Через NAT он почти
    /// всегда отличается от внешнего, и сервер дополняет его тем,
    /// что видит со своей стороны (см. [`ServerMessage::Registered`]).
    Register {
        /// ID, который хост показывает человеку.
        id: DeviceId,
        /// Локальный адрес, на котором хост слушает QUIC.
        local_addr: String,
        /// Внешний UDP-адрес, как его увидел STUN.
        ///
        /// # Почему сервер не может выяснить это сам
        ///
        /// Сервер видит наше WebSocket-соединение, то есть TCP, и
        /// знает его внешний порт. Внешний порт **UDP-сокета** у NAT
        /// другой, и связи между ними нет никакой.
        ///
        /// Пока сервер отдавал сторонам TCP-адрес, пробивание не
        /// работало через интернет вовсе: обе стороны били в порт,
        /// которого у собеседника нет (находка 65). На одной машине
        /// дефект не проявлялся — там стороны обмениваются
        /// локальными адресами, где порт настоящий.
        ///
        /// `None` означает «STUN не ответил» либо сообщение от старой
        /// версии. Тогда сервер отдаёт TCP-адрес, как раньше: хуже
        /// не станет, а в локальной сети это и не нужно.
        external_addr: Option<String>,
    },

    /// «Хочу подключиться к устройству с таким ID».
    Connect {
        /// ID, который человек ввёл в поле.
        id: DeviceId,
        /// Свой локальный адрес, чтобы хост знал, куда отвечать.
        local_addr: String,
        /// Внешний UDP-адрес, как его увидел STUN.
        ///
        /// Смысл тот же, что у [`ClientMessage::Register`]: без него
        /// хост бьёт в TCP-порт клиента, то есть в никуда.
        external_addr: Option<String>,
    },

    /// «Я жив» — чтобы сервер не считал хост отвалившимся.
    KeepAlive,
}

/// Сообщение от сервера к устройству.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    /// Регистрация принята.
    Registered {
        /// ID, под которым хост теперь известен.
        id: DeviceId,
        /// Адрес, с которого сервер увидел хоста.
        ///
        /// За NAT он отличается от локального, и именно его надо
        /// сообщать клиенту. Хост сам свой внешний адрес не знает —
        /// это и есть простейшая функция STUN.
        public_addr: String,
    },

    /// Пир найден, вот куда идти.
    PeerFound {
        /// ID найденного устройства.
        id: DeviceId,
        /// Адрес, на который стучаться.
        addr: String,
    },

    /// Кто-то хочет подключиться к нам.
    ///
    /// Приходит **хосту**, чтобы тот начал бить в сторону клиента
    /// одновременно с ним: пробивание NAT требует встречного
    /// движения с обеих сторон (§5.4).
    PeerWants {
        /// ID того, кто подключается.
        id: DeviceId,
        /// Его адрес.
        addr: String,
    },

    /// Отказ с человеческой причиной.
    Error {
        /// Что произошло — текст показывается человеку.
        reason: String,
    },
}

/// Разделитель полей. Табуляция, а не пробел: пробел встречается
/// внутри значений (в адресах нет, но правило дешевле исключения).
const SEP: char = '\t';

impl ClientMessage {
    /// Закодировать для отправки.
    pub fn encode(&self) -> String {
        match self {
            // Внешний адрес — необязательное четвёртое поле.
            //
            // Именно необязательное, а не новый вид сообщения:
            // стороны обновляются не одновременно, и старый клиент
            // должен продолжать работать с новым сервером. Пустая
            // строка вместо пропуска поля — чтобы разделителей всегда
            // было поровну и разбор не зависел от их числа.
            Self::Register {
                id,
                local_addr,
                external_addr,
            } => {
                format!(
                    "register{SEP}{}{SEP}{}{SEP}{}",
                    id.to_compact(),
                    local_addr,
                    external_addr.as_deref().unwrap_or("")
                )
            }
            Self::Connect {
                id,
                local_addr,
                external_addr,
            } => {
                format!(
                    "connect{SEP}{}{SEP}{}{SEP}{}",
                    id.to_compact(),
                    local_addr,
                    external_addr.as_deref().unwrap_or("")
                )
            }
            Self::KeepAlive => "ping".to_string(),
        }
    }

    /// Разобрать пришедшее от устройства.
    ///
    /// `None` на всё, что не разобралось. Это недоверенные данные:
    /// сообщение приходит от кого угодно до всякой проверки, и
    /// единственно верная реакция на непонятное — отбросить его,
    /// а не пытаться угадать намерение (§4.3.5, §8.5).
    pub fn parse(input: &str) -> Option<Self> {
        if input.len() > MAX_MESSAGE_LEN {
            return None;
        }

        let mut parts = input.split(SEP);
        match parts.next()? {
            "register" => {
                let id = DeviceId::parse(parts.next()?)?;
                let local_addr = validate_addr(parts.next()?)?;
                let external_addr = parse_optional_addr(parts.next())?;
                // Лишние поля — признак другой версии протокола или
                // подделки. Игнорировать их значит согласиться
                // работать с тем, чего мы не понимаем.
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::Register {
                    id,
                    local_addr,
                    external_addr,
                })
            }
            "connect" => {
                let id = DeviceId::parse(parts.next()?)?;
                let local_addr = validate_addr(parts.next()?)?;
                let external_addr = parse_optional_addr(parts.next())?;
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::Connect {
                    id,
                    local_addr,
                    external_addr,
                })
            }
            "ping" => {
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::KeepAlive)
            }
            _ => None,
        }
    }
}

impl ServerMessage {
    /// Закодировать для отправки.
    pub fn encode(&self) -> String {
        match self {
            Self::Registered { id, public_addr } => {
                format!("registered{SEP}{}{SEP}{}", id.to_compact(), public_addr)
            }
            Self::PeerFound { id, addr } => {
                format!("peer{SEP}{}{SEP}{}", id.to_compact(), addr)
            }
            Self::PeerWants { id, addr } => {
                format!("wants{SEP}{}{SEP}{}", id.to_compact(), addr)
            }
            Self::Error { reason } => {
                // Перевод строки в причине сломал бы построчный
                // разбор на той стороне: одно сообщение стало бы
                // двумя, второе — мусором.
                let clean = reason.replace(['\n', '\r', SEP], " ");
                format!("error{SEP}{clean}")
            }
        }
    }

    /// Разобрать пришедшее от сервера.
    ///
    /// Сервер тоже недоверен: он может быть подменён (§8.1), и до
    /// этапа 5 у нас нет способа это заметить. Разбор поэтому такой
    /// же строгий, как для клиентских сообщений.
    pub fn parse(input: &str) -> Option<Self> {
        if input.len() > MAX_MESSAGE_LEN {
            return None;
        }

        let mut parts = input.split(SEP);
        match parts.next()? {
            "registered" => {
                let id = DeviceId::parse(parts.next()?)?;
                let public_addr = validate_addr(parts.next()?)?;
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::Registered { id, public_addr })
            }
            "peer" => {
                let id = DeviceId::parse(parts.next()?)?;
                let addr = validate_addr(parts.next()?)?;
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::PeerFound { id, addr })
            }
            "wants" => {
                let id = DeviceId::parse(parts.next()?)?;
                let addr = validate_addr(parts.next()?)?;
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::PeerWants { id, addr })
            }
            "error" => {
                let reason = parts.next()?;
                if reason.is_empty() {
                    return None;
                }
                Some(Self::Error {
                    reason: reason.to_string(),
                })
            }
            _ => None,
        }
    }
}

/// Проверить, что строка похожа на `адрес:порт`.
///
/// Полный разбор здесь делать нельзя: `bd-core` не должен зависеть
/// от `std::net` ради портируемости, а главное — адрес всё равно
/// разбирает тот, кто будет по нему соединяться. Здесь отсекается
/// заведомый мусор, чтобы он не уехал дальше по системе.
fn validate_addr(s: &str) -> Option<String> {
    // Пусто или слишком длинно — не адрес. 64 хватает и на IPv6
    // с портом, и на имя хоста.
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    // Порт обязателен: адрес без порта бесполезен — стучаться некуда.
    if !s.contains(':') {
        return None;
    }
    // Управляющие символы в адресе означают либо порчу, либо попытку
    // подделать разбор на той стороне.
    if s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(s.to_string())
}

/// Разобрать необязательное поле адреса.
///
/// Три случая, и все три законны:
/// - поля нет вовсе (`None` на входе) — сообщение старой версии;
/// - поле пустое — сторона новая, но STUN не ответил;
/// - поле с адресом — проверяем его как обычный адрес.
///
/// Внешний `Option` в результате отличает «разобрали» от «мусор»:
/// вернуть `Some(None)` и `None` — разные исходы, и путать их
/// нельзя (тот же урок, что в находке 38б про два смысла `None`).
fn parse_optional_addr(field: Option<&str>) -> Option<Option<String>> {
    match field {
        None => Some(None),
        Some("") => Some(None),
        Some(s) => validate_addr(s).map(Some),
    }
}

impl fmt::Display for ServerMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.encode())
    }
}

/// Адрес STUN, выведенный из URL сигналинга.
///
/// STUN живёт на **том же хосте и том же порту**, что и WebSocket,
/// только по UDP (см. `bd_core::stun`). Отдельного адреса не
/// заводится намеренно: две настройки вместо одной означали бы, что
/// их можно рассогласовать, а человек, поднявший свой сервер, узнал
/// бы об этом только на неработающем пробивании.
///
/// Возвращает `None`, если URL не разбирается: тогда STUN просто не
/// используется, а соединение всё равно стоит попробовать.
pub fn stun_addr_from_url(url: &str) -> Option<String> {
    // `ws://host:port/path` → `host:port`. Разбор вручную: тянуть
    // крейт URL в bd-core ради одной строки нельзя (§4.2.1).
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))?;
    let authority = rest.split('/').next()?;
    if authority.is_empty() || !authority.contains(':') {
        return None;
    }
    Some(authority.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> DeviceId {
        DeviceId::from_u32(418_207_356).expect("валидный ID")
    }

    #[test]
    fn client_messages_survive_a_round_trip() {
        let cases = [
            ClientMessage::Register {
                id: id(),
                local_addr: "192.168.1.5:7000".into(),
                external_addr: Some("203.0.113.7:41234".into()),
            },
            // Без внешнего адреса: STUN не ответил или сторона старая.
            ClientMessage::Register {
                id: id(),
                local_addr: "192.168.1.5:7000".into(),
                external_addr: None,
            },
            ClientMessage::Connect {
                id: id(),
                local_addr: "10.0.0.2:51000".into(),
                external_addr: None,
            },
            ClientMessage::KeepAlive,
        ];
        for msg in cases {
            let encoded = msg.encode();
            assert_eq!(
                ClientMessage::parse(&encoded),
                Some(msg.clone()),
                "не пережило кодирование: {encoded}"
            );
        }
    }

    #[test]
    fn server_messages_survive_a_round_trip() {
        let cases = [
            ServerMessage::Registered {
                id: id(),
                public_addr: "203.0.113.7:7000".into(),
            },
            ServerMessage::PeerFound {
                id: id(),
                addr: "203.0.113.9:7000".into(),
            },
            ServerMessage::PeerWants {
                id: id(),
                addr: "[2001:db8::1]:7000".into(),
            },
            ServerMessage::Error {
                reason: "хост с таким ID не найден".into(),
            },
        ];
        for msg in cases {
            let encoded = msg.encode();
            assert_eq!(
                ServerMessage::parse(&encoded),
                Some(msg.clone()),
                "не пережило кодирование: {encoded}"
            );
        }
    }

    #[test]
    fn garbage_is_rejected_not_guessed() {
        // Недоверенный ввод. Каждая строка — то, что реально может
        // прийти: обрезанное сообщение, чужой протокол, попытка
        // подсунуть адрес без порта.
        for input in [
            "",
            "register",                       // нет полей
            "register\t418207356",            // нет адреса
            "register\t418207356\t",          // пустой адрес
            "register\t000000000\t1.2.3.4:1", // невалидный ID
            "register\t418207356\tбезпорта",  // адрес без порта
            "GET / HTTP/1.1",                 // чужой протокол
            "ping\tлишнее",                   // лишнее поле
            "неизвестно\t1\t2",
        ] {
            assert_eq!(ClientMessage::parse(input), None, "принято зря: {input:?}");
        }
    }

    #[test]
    fn oversized_messages_are_rejected() {
        // Без предела длины строка от постороннего заставила бы
        // сервер выделять память по чужой команде (§8.5).
        let huge = format!("register\t418207356\t{}", "a".repeat(MAX_MESSAGE_LEN));
        assert!(huge.len() > MAX_MESSAGE_LEN);
        assert_eq!(ClientMessage::parse(&huge), None);
    }

    #[test]
    fn error_reason_cannot_break_line_framing() {
        // Причина приходит в том числе из чужого ввода (имя, ID).
        // Перевод строки внутри неё разорвал бы одно сообщение на
        // два, и второе на той стороне стало бы мусором.
        //
        // Тест проверен на способность провалиться: без replace
        // разбор возвращает обрезанную причину.
        let msg = ServerMessage::Error {
            reason: "плохо\nregistered\t418207356\t1.2.3.4:5".into(),
        };
        let encoded = msg.encode();
        assert!(!encoded.contains('\n'), "перевод строки уцелел: {encoded}");

        let back = ServerMessage::parse(&encoded).expect("должно разобраться");
        match back {
            ServerMessage::Error { reason } => {
                assert!(!reason.contains('\n'));
                // И, главное, вложенное сообщение не стало отдельным.
                assert!(reason.starts_with("плохо"));
            }
            other => panic!("разобралось не в ошибку: {other:?}"),
        }
    }

    #[test]
    fn addresses_with_control_characters_are_rejected() {
        // Управляющий символ в адресе — либо порча, либо попытка
        // подделать разбор у собеседника.
        assert_eq!(
            ClientMessage::parse("register\t418207356\t1.2.3.4:7000\u{7}"),
            None
        );
    }

    #[test]
    fn stun_addr_follows_signaling_url() {
        assert_eq!(
            stun_addr_from_url("ws://89.168.99.202:9000/ws").as_deref(),
            Some("89.168.99.202:9000")
        );
        assert_eq!(
            stun_addr_from_url("wss://example.com:443/ws").as_deref(),
            Some("example.com:443")
        );
        // Вшитый адрес обязан разбираться: если он перестанет,
        // пробивание тихо останется без STUN.
        assert!(stun_addr_from_url(DEFAULT_SIGNALING).is_some());
    }

    #[test]
    fn stun_addr_rejects_garbage() {
        // Проверка обязана уметь отвергать (находка 4).
        assert_eq!(stun_addr_from_url("http://host:9000/ws"), None);
        assert_eq!(stun_addr_from_url("ws://host/ws"), None, "без порта");
        assert_eq!(stun_addr_from_url("ws:///ws"), None);
        assert_eq!(stun_addr_from_url(""), None);
    }

    #[test]
    fn old_clients_without_external_field_still_parse() {
        // Стороны обновляются не одновременно: клиент старой версии
        // шлёт три поля вместо четырёх, и сервер обязан его понять.
        // Иначе обновление сервера разом отключило бы всех, кто ещё
        // не обновился.
        let old = "register\t418207356\t192.168.1.5:7000";
        assert_eq!(
            ClientMessage::parse(old),
            Some(ClientMessage::Register {
                id: DeviceId::parse("418207356").unwrap(),
                local_addr: "192.168.1.5:7000".into(),
                external_addr: None,
            })
        );

        // Пустое четвёртое поле — новая версия, но STUN не ответил.
        let empty = "register\t418207356\t192.168.1.5:7000\t";
        assert!(matches!(
            ClientMessage::parse(empty),
            Some(ClientMessage::Register {
                external_addr: None,
                ..
            })
        ));
    }

    #[test]
    fn external_addr_is_validated_like_any_other() {
        // Поле приходит из сети и доверия не заслуживает (§8.5).
        // Проверка обязана уметь отвергать — иначе она ничего не
        // значит (находка 4).
        assert_eq!(
            ClientMessage::parse("register\t418207356\t1.2.3.4:7000\tбез-порта"),
            None
        );
        assert_eq!(
            ClientMessage::parse("register\t418207356\t1.2.3.4:7000\t1.2.3.4:80\u{7}"),
            None
        );
    }
}
