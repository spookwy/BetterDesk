//! Клиент сигналинга: найти пира по девятизначному ID.
//!
//! # Что это решает
//!
//! До сих пор обе стороны требовали, чтобы человек знал IP-адрес
//! собеседника и умел его вписать. Через интернет это вдобавок
//! означало ручной проброс портов на роутере.
//!
//! Здесь появляется то, ради чего сигналинг и заводился: **хост
//! называет девять цифр, клиент их вводит**, адреса стороны узнают
//! друг о друге сами.
//!
//! # Устройство: снова отдельный поток
//!
//! WebSocket требует async, а пайплайн синхронный (§4.4). Решение то
//! же, что у QUIC (ADR 0002): tokio живёт в своём потоке, наружу
//! торчит синхронный API через каналы. Причина та же — джиттер
//! планировщика не должен попадать в горячий путь кадра.
//!
//! Разница с QUIC в том, что здесь горячего пути нет вовсе:
//! сообщений единицы за сессию. Поток нужен не ради задержки, а
//! ради того, чтобы **не тащить async в вызывающего**.
//!
//! # Чему здесь нельзя верить
//!
//! Всё, что приходит от сервера, — недоверенные данные. Сервер может
//! быть подменён, и до этапа 5 заметить это нечем (§8.1). Поэтому
//! адрес, полученный отсюда, годится ровно на одно: попробовать
//! соединиться. Он не даёт никаких прав и ничего не подтверждает.

use bd_core::device::DeviceId;
use bd_core::signaling::{ClientMessage, ServerMessage};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::{Result, TransportError};

/// Как часто напоминать серверу о себе.
///
/// Втрое чаще, чем сервер считает хост протухшим (90 с): потеря
/// одного-двух пингов не должна выбрасывать хост из реестра.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(30);

/// Что случилось на сигналинге.
///
/// # Почему свой enum, а не `Option`
///
/// «Пока ничего» и «сервер отказал» требуют разной реакции: в первом
/// случае надо ждать дальше, во втором — показать человеку причину и
/// прекратить. `Option` их не различает, и вызывающий выходил бы из
/// цикла в обоих случаях — ровно тот дефект, что в находке 38(б).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalEvent {
    /// Регистрация принята; вот наш адрес, каким его видит сервер.
    Registered {
        /// ID, под которым мы теперь известны.
        id: DeviceId,
        /// Наш внешний адрес. За NAT отличается от локального.
        public_addr: String,
    },
    /// Пир найден — вот куда стучаться.
    PeerFound {
        /// ID пира.
        id: DeviceId,
        /// Адрес пира.
        addr: String,
    },
    /// К нам хотят подключиться. Хосту это сигнал начать встречное
    /// движение: без него NAT не пробивается.
    PeerWants {
        /// ID того, кто стучится.
        id: DeviceId,
        /// Его адрес.
        addr: String,
    },
    /// Сервер отказал, и вот человеческая причина.
    Failed {
        /// Текст, который показывается человеку.
        reason: String,
    },
}

/// Соединение с сигналингом.
///
/// Живёт всё время работы: хост держит его, чтобы его можно было
/// найти по ID, клиент — чтобы получить ответ.
pub struct Signaling {
    to_server: Sender<ClientMessage>,
    from_server: Receiver<SignalEvent>,
    connected: Arc<AtomicBool>,
    _worker: WorkerHandle,
}

/// Владелец потока. При уничтожении дожидается завершения — иначе
/// поток продолжил бы писать в закрытые каналы, а его сообщения
/// об ошибках появлялись бы после выхода из программы.
struct WorkerHandle {
    handle: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Signaling {
    /// Подключиться к сигналингу.
    ///
    /// `server` — адрес вида `ws://192.168.1.5:9000/ws`.
    ///
    /// Блокируется до установления соединения или таймаута: без
    /// сигналинга ни хосту, ни клиенту дальше делать нечего, и
    /// отложенная ошибка была бы хуже — человек ждал бы у пустого
    /// экрана, не зная, что связи нет.
    pub fn connect(server: &str, timeout: Duration) -> Result<Self> {
        let (to_server, outgoing) = crossbeam_channel::unbounded();
        let (incoming, from_server) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let connected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_connected = Arc::clone(&connected);
        let worker_shutdown = Arc::clone(&shutdown);
        let url = server.to_string();

        let handle = std::thread::Builder::new()
            .name("bd-signaling".into())
            .spawn(move || {
                run_worker(
                    url,
                    outgoing,
                    incoming,
                    worker_connected,
                    worker_shutdown,
                    ready_tx,
                );
            })
            .map_err(|e| TransportError::Setup(format!("поток сигналинга: {e}")))?;

        let worker = WorkerHandle {
            handle: Some(handle),
            shutdown,
        };

        // Результат рукопожатия приходит через канал: ошибку
        // соединения вызывающий должен увидеть сразу, а не узнать
        // по молчанию.
        match ready_rx.recv_timeout(timeout + Duration::from_secs(1)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(TransportError::Connect(
                    "сигналинг не ответил вовремя".into(),
                ))
            }
        }

        Ok(Self {
            to_server,
            from_server,
            connected,
            _worker: worker,
        })
    }

    /// Объявить себя хостом под этим ID.
    pub fn register(&self, id: DeviceId, local_addr: SocketAddr) -> Result<()> {
        self.send(ClientMessage::Register {
            id,
            local_addr: local_addr.to_string(),
        })
    }

    /// Попросить адрес хоста с этим ID.
    pub fn connect_to(&self, id: DeviceId, local_addr: SocketAddr) -> Result<()> {
        self.send(ClientMessage::Connect {
            id,
            local_addr: local_addr.to_string(),
        })
    }

    fn send(&self, msg: ClientMessage) -> Result<()> {
        self.to_server
            .send(msg)
            .map_err(|_| TransportError::ThreadGone)
    }

    /// Забрать событие, если оно есть. Не блокирует.
    pub fn poll(&self) -> Result<Option<SignalEvent>> {
        match self.from_server.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => {
                if self.connected.load(Ordering::Relaxed) {
                    Ok(None)
                } else {
                    Err(TransportError::Disconnected)
                }
            }
            Err(TryRecvError::Disconnected) => Err(TransportError::ThreadGone),
        }
    }

    /// Дождаться события, но не дольше `timeout`.
    ///
    /// Нужно там, где ждать больше нечего: клиент после запроса по ID
    /// не делает ничего, пока не придёт адрес. Опрос в цикле там
    /// сжигал бы процессор без всякой пользы.
    pub fn wait(&self, timeout: Duration) -> Result<Option<SignalEvent>> {
        match self.from_server.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if self.connected.load(Ordering::Relaxed) {
                    Ok(None)
                } else {
                    Err(TransportError::Disconnected)
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                Err(TransportError::ThreadGone)
            }
        }
    }

    /// Живо ли соединение с сервером.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Тело потока: async внутри, каналы наружу.
fn run_worker(
    url: String,
    outgoing: Receiver<ClientMessage>,
    incoming: Sender<SignalEvent>,
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    ready: Sender<Result<()>>,
) {
    // Однопоточный рантайм: соединение одно, сообщений единицы.
    // Многопоточный завёл бы пул потоков ради нечего (ADR 0002).
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(TransportError::Setup(format!("рантайм: {e}"))));
            return;
        }
    };

    runtime.block_on(async move {
        use futures_util::{SinkExt, StreamExt};

        let stream = match tokio_tungstenite::connect_async(&url).await {
            Ok((stream, _response)) => stream,
            Err(e) => {
                // Причина объясняется человеку: «не удалось
                // подключиться» без адреса и причины не даёт ему
                // ничего сделать.
                let _ = ready.send(Err(TransportError::Connect(format!(
                    "сигналинг {url} недоступен: {e}"
                ))));
                return;
            }
        };

        connected.store(true, Ordering::Relaxed);
        let _ = ready.send(Ok(()));

        let (mut write, mut read) = stream.split();
        let mut keepalive = tokio::time::interval(KEEPALIVE_EVERY);
        // Первый тик срабатывает сразу — пропускаем: мы только что
        // подключились, напоминать о себе рано.
        keepalive.tick().await;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            tokio::select! {
                // Раз в интервал напоминаем серверу, что живы.
                _ = keepalive.tick() => {
                    let msg = ClientMessage::KeepAlive.encode();
                    if write.send(tokio_tungstenite::tungstenite::Message::Text(msg))
                        .await.is_err()
                    {
                        break;
                    }
                }

                // Пришло от сервера.
                frame = read.next() => {
                    let Some(Ok(frame)) = frame else { break };

                    let text = match frame {
                        tokio_tungstenite::tungstenite::Message::Text(t) => t,
                        tokio_tungstenite::tungstenite::Message::Close(_) => break,
                        // Прочее (ping/pong/binary) нас не касается:
                        // ping/pong обрабатывает библиотека.
                        _ => continue,
                    };

                    // Недоверенные данные: сервер может быть подменён,
                    // и заметить это нечем до этапа 5 (§8.1).
                    let Some(msg) = ServerMessage::parse(&text) else {
                        tracing::debug!("непонятное сообщение от сигналинга");
                        continue;
                    };

                    let event = match msg {
                        ServerMessage::Registered { id, public_addr } =>
                            SignalEvent::Registered { id, public_addr },
                        ServerMessage::PeerFound { id, addr } =>
                            SignalEvent::PeerFound { id, addr },
                        ServerMessage::PeerWants { id, addr } =>
                            SignalEvent::PeerWants { id, addr },
                        ServerMessage::Error { reason } =>
                            SignalEvent::Failed { reason },
                    };

                    if incoming.send(event).is_err() {
                        break;
                    }
                }

                // Надо отправить наше сообщение.
                //
                // Опрос с паузой, а не блокирующее чтение: канал
                // синхронный (crossbeam), и ждать на нём внутри
                // async значило бы заблокировать весь рантайм —
                // включая приём, который идёт в этом же select.
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    while let Ok(msg) = outgoing.try_recv() {
                        let encoded = msg.encode();
                        if write.send(
                            tokio_tungstenite::tungstenite::Message::Text(encoded)
                        ).await.is_err() {
                            connected.store(false, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
        tracing::debug!("поток сигналинга завершён");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_server_fails_with_a_reason() {
        // Отказ должен объяснять, куда не удалось подключиться:
        // «не удалось» без адреса не даёт человеку ничего сделать.
        //
        // Порт 1 закрыт везде, соединение отвергается сразу.
        // `expect_err` требует Debug у Ok-типа, а `Signaling` его не
        // имеет намеренно: внутри каналы и поток, печатать там нечего.
        let text = match Signaling::connect("ws://127.0.0.1:1/ws", Duration::from_secs(2)) {
            Ok(_) => panic!("порт 1 не должен отвечать"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("127.0.0.1:1"), "в ошибке нет адреса: {text}");
    }

    #[test]
    fn events_are_distinguishable() {
        // «Пока ничего» и «сервер отказал» требуют разной реакции:
        // ждать дальше или показать причину и прекратить. Option
        // их не различал бы (находка 38б).
        let failed = SignalEvent::Failed {
            reason: "хост не в сети".into(),
        };
        let found = SignalEvent::PeerFound {
            id: DeviceId::from_u32(418_207_356).expect("валидный ID"),
            addr: "1.2.3.4:7000".into(),
        };
        assert_ne!(failed, found);
    }
}
