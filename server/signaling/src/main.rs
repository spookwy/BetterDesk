//! Сигналинг BetterDesk: хосты объявляются по ID, стороны находят
//! друг друга.
//!
//! # Что этот сервер делает
//!
//! Помнит, какой ID у какого адреса, и сводит две стороны. Всё.
//! Видео, звук и ввод идут **напрямую между машинами** и через
//! сервер не проходят (CLAUDE.md §3.1) — поэтому он умещается в
//! бесплатный VPS и обслуживает тысячи клиентов при десятках
//! мегабайт памяти.
//!
//! # Чего он НЕ делает
//!
//! Не аутентифицирует. Сейчас любой может назваться любым ID и
//! забрать чужой адрес — это записанный временный компромисс до
//! этапа 5, где появляются ключи устройств (§7.3). Отсюда правило,
//! которое надо помнить, читая этот код: **всё, что приходит по
//! сети, недоверенно**, и ни одно значение отсюда не должно
//! попадать никуда без проверки.
//!
//! Не проверяет подписку. Это придёт на этап 4+ вместе с JWT
//! (§8.3), и именно здесь — потому что проверка на сервере
//! единственная, которую нельзя обойти патчем клиента.
//!
//! # Зачем серверу знать внешний адрес
//!
//! Хост за NAT не знает своего внешнего адреса: он видит `192.168.x`,
//! а из интернета доступен как `203.0.113.x` с другим портом. Узнать
//! это можно только у того, кто смотрит снаружи. Сервер как раз
//! снаружи и смотрит — так работает STUN, и отдельного STUN-сервера
//! нам для этого не нужно.
//!
//! Запуск: `cargo run --release -p bd-signaling -- --listen 0.0.0.0:9000`

#![forbid(unsafe_code)]

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bd_core::device::DeviceId;
use bd_core::signaling::{ClientMessage, ServerMessage};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};

/// Сколько хост считается живым без единого сообщения.
///
/// Клиент шлёт `ping` втрое чаще, поэтому потеря одного-двух
/// не выбрасывает хоста из реестра. Короче делать нельзя: у людей
/// бывают паузы в сети, а исчезнувший из списка хост выглядит как
/// поломка продукта.
const HOST_TTL: Duration = Duration::from_secs(90);

/// Как часто выметать протухшие записи.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// Предел записей в реестре.
///
/// Регистрация ничем не защищена (аутентификация — этап 5), поэтому
/// без предела один человек мог бы занять всю память сервера,
/// зарегистрировав миллион ID. Это не гипотеза: сервер стоит в
/// интернете открытым портом.
const MAX_HOSTS: usize = 100_000;

/// Зарегистрированный хост.
#[derive(Debug, Clone)]
struct Host {
    /// Адрес, с которого сервер увидел хоста (внешний, за NAT).
    public_addr: SocketAddr,
    /// Адрес, который хост назвал сам (локальный, для LAN).
    ///
    /// Нужен, когда обе стороны в одной сети: там внешние адреса
    /// совпадают, и стучаться надо по локальному.
    local_addr: String,
    /// Внешний UDP-адрес от STUN. `None` — старая версия или
    /// STUN не ответил; тогда используется адрес TCP-соединения.
    external_addr: Option<String>,
    /// Когда хост последний раз давал о себе знать.
    seen: Instant,
    /// Куда слать сообщения этому хосту.
    tx: mpsc::UnboundedSender<ServerMessage>,
}

/// Реестр живых хостов.
type Registry = Arc<RwLock<HashMap<DeviceId, Host>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let listen: SocketAddr = parse_listen().unwrap_or_else(|| "0.0.0.0:9000".parse().unwrap());

    let registry: Registry = Arc::new(RwLock::new(HashMap::new()));

    // Уборка протухших записей. Без неё реестр растёт вечно: хост,
    // у которого выдернули провод, никогда не скажет «я ушёл».
    {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SWEEP_EVERY);
            loop {
                ticker.tick().await;
                let mut guard = registry.write().await;
                let before = guard.len();
                guard.retain(|_, host| host.seen.elapsed() < HOST_TTL);
                let removed = before - guard.len();
                if removed > 0 {
                    tracing::info!(removed, live = guard.len(), "выметены протухшие хосты");
                }
            }
        });
    }

    // UDP-STUN на том же порту, что и WebSocket.
    //
    // # Зачем он нужен отдельно от WebSocket
    //
    // Сервер видит клиента по TCP-соединению и знает его внешний
    // **TCP**-порт. Долгое время он его и отдавал сторонам как адрес
    // для пробивания NAT — и на одной машине это работало, потому
    // что там стороны обмениваются локальными адресами.
    //
    // Через интернет первый же прогон провалился (находка 65):
    // пробивание и QUIC идут по UDP, у которого свой внешний порт,
    // и стороны били в TCP-порт друг друга. Внешний UDP-порт знает
    // только NAT, и спросить его можно единственным способом —
    // послать пакет с того самого сокета и узнать, каким его увидели.
    //
    // Тот же порт, а не соседний: иначе в файрволе VPS пришлось бы
    // открывать второй, а инструкция по развёртыванию — это то, что
    // человек выполняет один раз и не перечитывает.
    {
        let udp = tokio::net::UdpSocket::bind(listen).await?;
        tracing::info!(%listen, "STUN слушает UDP");
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let (len, from) = match udp.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        // Одна неудача не повод ронять слушателя:
                        // на UDP ошибка часто описывает прошлый
                        // пакет, а не состояние сокета (та же
                        // причина, что у WSAECONNRESET в punch.rs).
                        tracing::debug!(?e, "приём STUN");
                        continue;
                    }
                };
                if !bd_core::stun::is_request(&buf[..len]) {
                    // Не наш пакет. Молча игнорируем: на публичный
                    // порт прилетает что угодно, и отвечать всем
                    // подряд значит работать усилителем для атак.
                    tracing::trace!(%from, len, "не STUN-запрос");
                    continue;
                }
                let reply = bd_core::stun::encode_reply(&from);
                if let Err(e) = udp.send_to(&reply, from).await {
                    tracing::debug!(?e, %from, "ответ STUN не ушёл");
                }
            }
        });
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(Arc::clone(&registry));

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "сигналинг слушает");
    println!("=== BetterDesk — сигналинг ===");
    println!("Слушаю {listen}");
    println!("Проверка живости: http://{listen}/health");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("остановка по Ctrl+C");
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(registry): State<Registry>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, peer, registry))
}

/// Обслужить одно соединение.
///
/// Соединение живёт, пока живёт устройство: хост держит его всё
/// время работы, чтобы его можно было найти по ID.
async fn handle_socket(mut socket: WebSocket, peer: SocketAddr, registry: Registry) {
    tracing::debug!(%peer, "соединение установлено");

    // Канал для сообщений этому устройству. Нужен потому, что писать
    // в сокет может не только его собственный обработчик: когда
    // клиент ищет хоста, хосту надо сказать «к тебе стучатся» —
    // а это другое соединение и другая задача.
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    // ID, под которым это устройство зарегистрировалось. Нужен,
    // чтобы убрать запись при разрыве: иначе клиенты получали бы
    // адрес хоста, которого уже нет.
    let mut registered_as: Option<DeviceId> = None;

    loop {
        tokio::select! {
            // Сообщение нам от другой задачи (например, «к тебе стучатся»).
            Some(outgoing) = rx.recv() => {
                if socket.send(Message::Text(outgoing.encode().into())).await.is_err() {
                    break;
                }
            }

            // Сообщение от устройства.
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break };

                let text = match msg {
                    Message::Text(t) => t,
                    // Пинги протокола WebSocket axum обрабатывает сам.
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    // Двоичных сообщений в этом протоколе нет.
                    // Молча игнорировать нельзя: это признак либо
                    // другой версии, либо чужого клиента.
                    Message::Binary(_) => {
                        let _ = socket.send(Message::Text(
                            ServerMessage::Error {
                                reason: "ожидается текстовое сообщение".into(),
                            }
                            .encode()
                            .into(),
                        )).await;
                        continue;
                    }
                };

                let Some(parsed) = ClientMessage::parse(&text) else {
                    // Непонятое сообщение не рвёт соединение: это
                    // может быть клиент другой версии, и ему полезнее
                    // получить объяснение, чем молчаливый разрыв.
                    tracing::debug!(%peer, "непонятное сообщение");
                    let _ = socket.send(Message::Text(
                        ServerMessage::Error {
                            reason: "сообщение не разобрано".into(),
                        }
                        .encode()
                        .into(),
                    )).await;
                    continue;
                };

                let reply = handle_message(parsed, peer, &registry, &tx, &mut registered_as).await;

                if let Some(reply) = reply {
                    if socket.send(Message::Text(reply.encode().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Запись убирается при разрыве. Без этого клиент получал бы
    // адрес хоста, который уже ушёл, и упирался бы в таймаут вместо
    // честного «хост не в сети».
    if let Some(id) = registered_as {
        let mut guard = registry.write().await;
        // Сверка отправителя: за время нашего сна под этим ID мог
        // зарегистрироваться кто-то другой, и убрать чужую запись
        // значило бы выкинуть живой хост.
        if guard.get(&id).is_some_and(|h| h.tx.same_channel(&tx)) {
            guard.remove(&id);
            tracing::info!(%id, live = guard.len(), "хост отключился");
        }
    }

    tracing::debug!(%peer, "соединение закрыто");
}

/// Обработать одно разобранное сообщение.
///
/// Возвращает ответ, который надо послать отправителю (если нужен).
async fn handle_message(
    msg: ClientMessage,
    peer: SocketAddr,
    registry: &Registry,
    tx: &mpsc::UnboundedSender<ServerMessage>,
    registered_as: &mut Option<DeviceId>,
) -> Option<ServerMessage> {
    match msg {
        ClientMessage::Register {
            id,
            local_addr,
            external_addr,
        } => {
            let mut guard = registry.write().await;

            // Предел реестра. Регистрация ничем не защищена до
            // этапа 5, и без предела один человек занял бы всю
            // память сервера.
            if guard.len() >= MAX_HOSTS && !guard.contains_key(&id) {
                tracing::warn!(%peer, "реестр переполнен");
                return Some(ServerMessage::Error {
                    reason: "сервер перегружен, попробуйте позже".into(),
                });
            }

            // Перерегистрация под тем же ID разрешена: хост
            // перезапустился, и его прежняя запись ещё не протухла.
            // Отказывать значило бы заставлять человека ждать TTL.
            //
            // Дыра здесь настоящая и записанная: чужой может
            // перехватить ID, зарегистрировавшись под ним. Закрывается
            // на этапе 5 подписью ключом устройства (§7.3) — до тех
            // пор сигналингу верить нельзя.
            guard.insert(
                id,
                Host {
                    public_addr: peer,
                    local_addr,
                    external_addr,
                    seen: Instant::now(),
                    tx: tx.clone(),
                },
            );
            *registered_as = Some(id);

            tracing::info!(%id, %peer, live = guard.len(), "хост зарегистрирован");

            Some(ServerMessage::Registered {
                id,
                public_addr: peer.to_string(),
            })
        }

        ClientMessage::Connect {
            id,
            local_addr,
            external_addr,
        } => {
            let guard = registry.read().await;

            let Some(host) = guard.get(&id) else {
                // Честное сообщение вместо тихого зависания —
                // прямое требование этапа 4.
                tracing::debug!(%id, %peer, "хост не найден");
                return Some(ServerMessage::Error {
                    reason: format!("хост {id} не в сети"),
                });
            };

            if host.seen.elapsed() >= HOST_TTL {
                return Some(ServerMessage::Error {
                    reason: format!("хост {id} давно не выходил на связь"),
                });
            }

            // Какой адрес хоста отдавать клиенту: локальный или
            // внешний.
            //
            // Если внешние адреса совпадают, обе стороны за одним
            // NAT — то есть в одной локальной сети. Там стучаться по
            // внешнему адресу бесполезно: большинство домашних
            // роутеров не разворачивают пакет обратно внутрь
            // (это называется NAT hairpinning, и его часто нет).
            let same_nat = host.public_addr.ip() == peer.ip();
            let host_addr = if same_nat {
                host.local_addr.clone()
            } else {
                // Через интернет отдаём внешний UDP-адрес от STUN,
                // а `public_addr` (адрес TCP-соединения) — только
                // если STUN не ответил.
                //
                // Разница не косметическая: у TCP и UDP разные
                // внешние порты, и пока отдавался TCP-адрес,
                // пробивание не работало вовсе (находка 65).
                host.external_addr
                    .clone()
                    .unwrap_or_else(|| host.public_addr.to_string())
            };

            // Хосту сообщаем, что к нему стучатся, и даём адрес
            // клиента. Без этого пробить NAT нельзя: нужно встречное
            // движение с обеих сторон одновременно (§5.4).
            let client_addr = if same_nat {
                local_addr
            } else {
                // То же для клиента: хост будет бить в этот адрес,
                // и TCP-порт здесь бесполезен.
                external_addr.unwrap_or_else(|| peer.to_string())
            };

            let _ = host.tx.send(ServerMessage::PeerWants {
                id,
                addr: client_addr,
            });

            tracing::info!(%id, %peer, same_nat, "стороны сведены");

            Some(ServerMessage::PeerFound {
                id,
                addr: host_addr,
            })
        }

        ClientMessage::KeepAlive => {
            // Отметка живости. Без неё хост вымело бы через TTL,
            // хотя он никуда не девался.
            if let Some(id) = *registered_as {
                if let Some(host) = registry.write().await.get_mut(&id) {
                    host.seen = Instant::now();
                }
            }
            None
        }
    }
}

fn parse_listen() -> Option<SocketAddr> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--listen" {
            return args.next().and_then(|v| v.parse().ok());
        }
    }
    None
}
