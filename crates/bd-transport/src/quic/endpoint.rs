//! Соединение QUIC и мост между async-сетью и синхронным пайплайном.

use super::config;
use crate::fragment::{Fragmenter, ReassembledFrame, Reassembler, ReceiveOutcome};
use crate::loopback::TransportStats;
use crate::packet::PayloadKind;
use crate::{Result, TransportError};
use bd_core::metrics::{FrameTimings, Stage};
use bd_core::time::Epoch;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Сколько кадров держать в очереди на отправку.
///
/// Предел в кадрах, а не в байтах: так видно, сколько задержки он
/// допускает. Восемь кадров при 60 fps — это 133 мс, уже много;
/// больше копить бессмысленно, потому что устаревший кадр не нужен
/// (CLAUDE.md §5.3).
const SEND_QUEUE_FRAMES: usize = 8;

/// Сколько собранных кадров держать на приёме.
///
/// Меньше, чем на отправке: здесь кадры уже собраны и ждут только
/// декодера, который забирает их каждую итерацию. Глубокая очередь
/// тут означала бы, что пайплайн встал, — и лишние кадры всё равно
/// придётся выбросить.
const RECV_QUEUE_FRAMES: usize = 4;

/// Роль стороны в соединении.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Хост: слушает и принимает подключение.
    Host,
    /// Клиент: подключается к хосту.
    Client,
}

/// Кадр, ожидающий отправки.
struct Outgoing {
    kind: PayloadKind,
    keyframe: bool,
    data: Vec<u8>,
    /// Номер кадра.
    ///
    /// Присваивается **при постановке в очередь**, а не в потоке
    /// сети. Иначе вызывающий не узнал бы номер того кадра, который
    /// только что отправил, — а он нужен, чтобы сверить пришедший
    /// кадр с оригиналом (находка 27).
    sequence: u64,
    /// Момент постановки в очередь.
    ///
    /// Нужен, чтобы отметить [`Stage::Sent`] тем временем, когда кадр
    /// действительно ушёл в сеть, а не когда его положили в канал.
    queued_at: bd_core::time::Timestamp,
    /// Когда кадр был захвачен, в микросекундах нашей эпохи.
    ///
    /// Уезжает в заголовок каждого фрагмента: без этого приёмник не
    /// знает, когда кадр родился, и glass-to-glass между машинами
    /// посчитать нечем (находка 40).
    captured_at_micros: u64,
}

/// Счётчики, разделяемые между потоками.
///
/// Атомарные, а не под мьютексом: они обновляются на каждом датаграме,
/// и блокировка в этом пути стоила бы дороже самой статистики.
#[derive(Debug, Default)]
struct SharedStats {
    frames_sent: AtomicU64,
    frames_received: AtomicU64,
    frames_lost: AtomicU64,
    datagrams_sent: AtomicU64,
    datagrams_dropped: AtomicU64,
    bytes_sent: AtomicU64,
    /// Круговое время в микросекундах, как его измеряет QUIC.
    rtt_micros: AtomicU64,
    /// Живо ли соединение.
    connected: AtomicBool,
}

/// Транспорт поверх QUIC.
///
/// API совпадает с [`crate::LoopbackTransport`]: `send`, `receive`,
/// `stats`. Это позволяет пайплайну не знать, какой транспорт под ним,
/// и оставляет заглушку годной для замеров без сети.
pub struct QuicTransport {
    to_network: Sender<Outgoing>,
    from_network: Receiver<ReassembledFrame>,
    stats: Arc<SharedStats>,
    epoch: Epoch,
    /// Номер следующего кадра.
    ///
    /// Ведётся здесь, а не в потоке сети: вызывающему номер нужен
    /// сразу, чтобы запомнить отправленный кадр.
    next_sequence: u64,
    /// Держит поток живым. При уничтожении транспорта поток
    /// завершается, потому что каналы закрываются.
    _worker: WorkerHandle,
}

/// Владелец потока транспорта.
///
/// При уничтожении дожидается завершения: иначе поток продолжил бы
/// работать с закрытыми каналами, а его сообщения об ошибках
/// появлялись бы уже после выхода из программы.
struct WorkerHandle {
    handle: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // Поток проверяет флаг раз в 10 мс, поэтому ждать долго
            // не придётся. Игнорировать панику потока здесь нечем:
            // мы всё равно завершаемся.
            let _ = handle.join();
        }
    }
}

impl QuicTransport {
    /// Поднять хост и дождаться подключения клиента.
    ///
    /// Блокируется до подключения или таймаута: до появления клиента
    /// хосту всё равно нечего делать. Это единственное место, где
    /// транспорт блокирует вызывающего.
    pub fn host(bind: SocketAddr, accept_timeout: Duration, epoch: Epoch) -> Result<Self> {
        Self::spawn(Role::Host, bind, None, accept_timeout, epoch)
    }

    /// Подключиться к хосту.
    pub fn connect(server: SocketAddr, timeout: Duration, epoch: Epoch) -> Result<Self> {
        // Порт 0 — любой свободный: клиенту фиксированный не нужен.
        let bind = match server {
            SocketAddr::V4(_) => "0.0.0.0:0".parse(),
            SocketAddr::V6(_) => "[::]:0".parse(),
        }
        .map_err(|e| TransportError::Setup(format!("адрес привязки: {e}")))?;

        Self::spawn(Role::Client, bind, Some(server), timeout, epoch)
    }

    /// Общая часть: поднять поток и дождаться соединения.
    fn spawn(
        role: Role,
        bind: SocketAddr,
        server: Option<SocketAddr>,
        timeout: Duration,
        epoch: Epoch,
    ) -> Result<Self> {
        let (to_network, outgoing) = crossbeam_channel::bounded(SEND_QUEUE_FRAMES);
        let (incoming, from_network) = crossbeam_channel::bounded(RECV_QUEUE_FRAMES);
        let stats = Arc::new(SharedStats::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        // Результат установления соединения возвращается через канал:
        // ошибку рукопожатия вызывающий должен увидеть сразу, а не
        // узнать по молчанию транспорта.
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let worker_stats = Arc::clone(&stats);
        let worker_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("bd-quic".into())
            .spawn(move || {
                run_worker(
                    role,
                    bind,
                    server,
                    outgoing,
                    incoming,
                    worker_stats,
                    worker_shutdown,
                    ready_tx,
                );
            })
            .map_err(|e| TransportError::Setup(format!("поток транспорта: {e}")))?;

        let worker = WorkerHandle {
            handle: Some(handle),
            shutdown,
        };

        // Ждём результата рукопожатия. Таймаут чуть больше заданного:
        // поток сам следит за своим, и его сообщение об ошибке
        // информативнее нашего «не дождались».
        match ready_rx.recv_timeout(timeout + Duration::from_secs(1)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(TransportError::Connect(
                    "поток транспорта не ответил вовремя".into(),
                ))
            }
        }

        stats.connected.store(true, Ordering::Relaxed);

        Ok(Self {
            to_network,
            from_network,
            stats,
            epoch,
            next_sequence: 0,
            _worker: worker,
        })
    }

    /// Отправить кадр.
    ///
    /// **Не блокирует.** Если очередь полна — сеть не успевает, и
    /// кадр отбрасывается с [`TransportError::WouldBlock`]. Ждать
    /// нельзя: ожидание в потоке захвата остановило бы захват, а
    /// устаревший кадр всё равно не нужен (CLAUDE.md §5.3).
    ///
    /// Отметка [`Stage::Sent`] ставится **здесь**, а не в потоке
    /// сети: с точки зрения пайплайна кадр ушёл, когда его приняли
    /// к отправке. Время, проведённое в очереди, попадёт в стадию
    /// `network` — там ему и место, потому что это цена канала.
    pub fn send(
        &mut self,
        kind: PayloadKind,
        keyframe: bool,
        data: &[u8],
        timings: &mut FrameTimings,
    ) -> Result<u64> {
        if !self.stats.connected.load(Ordering::Relaxed) {
            return Err(TransportError::Disconnected);
        }

        let now = self.epoch.stamp_now();
        timings.mark(Stage::Sent, now);

        let sequence = self.next_sequence;

        let outgoing = Outgoing {
            kind,
            keyframe,
            data: data.to_vec(),
            queued_at: now,
            sequence,
            // Метка захвата уже стоит в таймингах — её поставил
            // `bd-capture`. Ноль означает «кадр не из захвата»
            // (ввод, курсор), и приёмник по нему задержку не считает.
            captured_at_micros: timings.get(Stage::Captured).map_or(0, |t| t.as_micros()),
        };

        match self.to_network.try_send(outgoing) {
            Ok(()) => {
                // Номер растёт только после успешной постановки:
                // отброшенный кадр не должен занимать номер, иначе
                // приёмник посчитает его потерянным и запросит
                // ключевой кадр без повода.
                self.next_sequence += 1;
                Ok(sequence)
            }
            Err(TrySendError::Full(_)) => Err(TransportError::WouldBlock {
                queued: self.to_network.len(),
            }),
            Err(TrySendError::Disconnected(_)) => Err(TransportError::ThreadGone),
        }
    }

    /// Забрать собранный кадр, если он есть.
    ///
    /// **Не блокирует.** `None` означает «пока нечего», а не ошибку:
    /// на приёме кадры появляются в своём темпе.
    ///
    /// Отметка [`Stage::Received`] ставится в момент выдачи, а не
    /// сборки: между ними кадр лежит в канале, и это время — часть
    /// пути, которую нельзя терять из замера.
    pub fn receive(&mut self, timings: &mut FrameTimings) -> Result<Option<ReassembledFrame>> {
        match self.from_network.try_recv() {
            Ok(frame) => {
                timings.mark(Stage::Received, self.epoch.stamp_now());
                Ok(Some(frame))
            }
            Err(TryRecvError::Empty) => {
                if self.stats.connected.load(Ordering::Relaxed) {
                    Ok(None)
                } else {
                    Err(TransportError::Disconnected)
                }
            }
            Err(TryRecvError::Disconnected) => Err(TransportError::ThreadGone),
        }
    }

    /// Дождаться кадра, но не дольше `timeout`.
    ///
    /// # Зачем нужен, если есть `receive`
    ///
    /// `receive` не блокирует, и вызывающий обязан сам решить, чем
    /// занять время до следующего кадра. У пробы `loopback` этим
    /// занятием оказался **захват собственного экрана**: цикл общий
    /// на обе роли, и приёмник ждал `AcquireNextFrame` с таймаутом
    /// 16 мс, хотя захватывать ему нечего.
    ///
    /// Цена этого измерена (находка 41): около 20 мс из 21 сидели в
    /// стадии `network` на localhost, где сети фактически нет. Кадр
    /// приходил в канал вовремя, но забирали его на следующем такте
    /// чужого цикла.
    ///
    /// Попытка снять таймаут захвата сделала **хуже** (33.5 мс):
    /// цикл без ожидания вытесняет поток QUIC, которому и надо
    /// доставить датаграмы. То есть лечится это не таймаутом захвата,
    /// а ожиданием **на том канале, откуда кадры приходят**, — что
    /// и делает этот метод.
    ///
    /// Ожидание отдаёт процессор: `recv_timeout` паркует поток, а не
    /// крутит опрос. Для клиента, которому больше нечего делать,
    /// это правильный способ ждать.
    ///
    /// `Ok(None)` — таймаут истёк, кадра нет. Это не ошибка: на
    /// статичном экране хост честно ничего не шлёт, а окно всё равно
    /// обязано жить (находка 37).
    pub fn receive_timeout(
        &mut self,
        timeout: Duration,
        timings: &mut FrameTimings,
    ) -> Result<Option<ReassembledFrame>> {
        match self.from_network.recv_timeout(timeout) {
            Ok(frame) => {
                timings.mark(Stage::Received, self.epoch.stamp_now());
                Ok(Some(frame))
            }
            Err(RecvTimeoutError::Timeout) => {
                if self.stats.connected.load(Ordering::Relaxed) {
                    Ok(None)
                } else {
                    Err(TransportError::Disconnected)
                }
            }
            Err(RecvTimeoutError::Disconnected) => Err(TransportError::ThreadGone),
        }
    }

    /// Счётчики канала.
    pub fn stats(&self) -> TransportStats {
        TransportStats {
            frames_sent: self.stats.frames_sent.load(Ordering::Relaxed),
            frames_received: self.stats.frames_received.load(Ordering::Relaxed),
            frames_lost: self.stats.frames_lost.load(Ordering::Relaxed),
            datagrams_sent: self.stats.datagrams_sent.load(Ordering::Relaxed),
            datagrams_dropped: self.stats.datagrams_dropped.load(Ordering::Relaxed),
            bytes_sent: self.stats.bytes_sent.load(Ordering::Relaxed),
        }
    }

    /// Круговое время, измеренное QUIC.
    ///
    /// Настоящий RTT соединения, а не наша оценка: quinn считает его
    /// по подтверждениям. Нужен для оверлея (критерий этапа 3) и для
    /// контроллера битрейта (этап 6).
    pub fn rtt(&self) -> Duration {
        Duration::from_micros(self.stats.rtt_micros.load(Ordering::Relaxed))
    }

    /// Живо ли соединение.
    pub fn is_connected(&self) -> bool {
        self.stats.connected.load(Ordering::Relaxed)
    }
}

/// Тело потока транспорта.
#[allow(clippy::too_many_arguments)]
fn run_worker(
    role: Role,
    bind: SocketAddr,
    server: Option<SocketAddr>,
    outgoing: Receiver<Outgoing>,
    incoming: Sender<ReassembledFrame>,
    stats: Arc<SharedStats>,
    shutdown: Arc<AtomicBool>,
    ready: Sender<Result<()>>,
) {
    // Однопоточный рантайм: у нас одно соединение, а не тысячи.
    // Многопоточный завёл бы пул воркеров, которому нечего делать.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(TransportError::Setup(format!("рантайм tokio: {e}"))));
            return;
        }
    };

    runtime.block_on(async move {
        let connection = match establish(role, bind, server).await {
            Ok(conn) => {
                let _ = ready.send(Ok(()));
                conn
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };

        tracing::info!(
            ?role,
            remote = %connection.remote_address(),
            "QUIC-соединение установлено"
        );

        pump(connection, outgoing, incoming, stats, shutdown).await;
    });
}

/// Установить соединение согласно роли.
async fn establish(
    role: Role,
    bind: SocketAddr,
    server: Option<SocketAddr>,
) -> Result<quinn::Connection> {
    match role {
        Role::Host => {
            let (server_config, _cert) = config::server_config()?;
            let endpoint = quinn::Endpoint::server(server_config, bind)
                .map_err(|e| TransportError::Setup(format!("привязка к {bind}: {e}")))?;

            tracing::info!(%bind, "хост ждёт подключения");

            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| TransportError::Connect("точка входа закрыта".into()))?;

            incoming
                .await
                .map_err(|e| TransportError::Connect(format!("рукопожатие: {e}")))
        }
        Role::Client => {
            let server = server
                .ok_or_else(|| TransportError::Setup("клиенту не задан адрес сервера".into()))?;

            let mut endpoint = quinn::Endpoint::client(bind)
                .map_err(|e| TransportError::Setup(format!("привязка клиента: {e}")))?;
            endpoint.set_default_client_config(config::client_config()?);

            // Имя сервера для TLS. Проверять его сейчас некому
            // (см. config.rs), но передать обязано: без него
            // рукопожатие не начнётся.
            endpoint
                .connect(server, "betterdesk")
                .map_err(|e| TransportError::Connect(format!("подключение к {server}: {e}")))?
                .await
                .map_err(|e| TransportError::Connect(format!("рукопожатие с {server}: {e}")))
        }
    }
}

/// Основной цикл: отправка и приём датаграмов.
async fn pump(
    connection: quinn::Connection,
    outgoing: Receiver<Outgoing>,
    incoming: Sender<ReassembledFrame>,
    stats: Arc<SharedStats>,
    shutdown: Arc<AtomicBool>,
) {
    // Предел датаграма выясняется у соединения, а не берётся
    // константой: он зависит от пути и может оказаться меньше нашего
    // MTU 1200. Слать больше — значит получать отказ на каждом кадре.
    let max_datagram = connection
        .max_datagram_size()
        .unwrap_or(crate::DEFAULT_MAX_PAYLOAD + crate::HEADER_SIZE);
    let payload_limit = max_datagram.saturating_sub(crate::HEADER_SIZE);

    tracing::info!(max_datagram, payload_limit, "размер датаграма согласован");

    let fragmenter = Fragmenter::with_max_payload(payload_limit.max(1));
    // Ёмкость сборщика: сколько кадров собирать одновременно.
    // Три — как в заглушке: при переупорядочивании фрагменты соседних
    // кадров перемешиваются, но не больше чем на пару кадров.
    let mut reassembler = Reassembler::new(3);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Обновление RTT: дёшево и нужно оверлею каждый кадр.
        stats
            .rtt_micros
            .store(connection.rtt().as_micros() as u64, Ordering::Relaxed);

        tokio::select! {
            // Приём датаграма из сети.
            received = connection.read_datagram() => {
                match received {
                    Ok(bytes) => {
                        match reassembler.accept(&bytes) {
                            Ok(ReceiveOutcome::Frame(frame)) => {
                                stats.frames_received.fetch_add(1, Ordering::Relaxed);
                                stats
                                    .frames_lost
                                    .store(reassembler.lost_frames(), Ordering::Relaxed);

                                // Полная очередь означает, что пайплайн
                                // не забирает кадры. Ждать нельзя —
                                // это остановило бы приём остальных
                                // датаграмов, и посыпались бы кадры,
                                // которые ещё можно было собрать.
                                if incoming.try_send(*frame).is_err() {
                                    tracing::trace!("приёмная очередь полна, кадр отброшен");
                                }
                            }
                            Ok(_) => {}
                            // Мусорный датаграм не рвёт сессию: он мог
                            // прийти от кого угодно (CLAUDE.md §8.5).
                            Err(e) if e.is_recoverable() => {
                                tracing::trace!(?e, "датаграм отброшен");
                            }
                            Err(e) => {
                                tracing::warn!(?e, "сборщик отказал");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::info!(?e, "соединение закрыто");
                        break;
                    }
                }
            }

            // Отправка кадра из очереди пайплайна.
            //
            // `recv` блокирующий, поэтому вызывается в blocking-задаче:
            // иначе он остановил бы весь рантайм, включая приём.
            // Таймаут нужен, чтобы цикл проверял флаг остановки даже
            // когда пайплайн ничего не шлёт.
            frame = recv_async(&outgoing) => {
                let frame = match frame {
                    PollOutgoing::Frame(frame) => *frame,
                    // Очередь пуста — это норма, а не повод выходить.
                    // Возвращаемся в `select!`: там ждёт приём.
                    PollOutgoing::Idle => continue,
                    PollOutgoing::Closed => {
                        tracing::debug!("очередь отправки закрыта, пайплайн завершился");
                        break;
                    }
                };

                let mut timings = FrameTimings::new(frame.sequence);
                timings.mark(Stage::Sent, frame.queued_at);

                // Номер берётся ИЗ КАДРА, а не из счётчика потока.
                //
                // Свой счётчик здесь разошёлся бы с тем, что видит
                // вызывающий: `send` не увеличивает номер, когда
                // очередь полна и кадр отброшен. Приёмник тогда
                // решил бы, что кадры пропали, и запрашивал бы
                // ключевой без повода. Расхождение тихое: пока
                // ничего не отбрасывается, номера совпадают.
                let result = fragmenter.fragment(
                    frame.kind,
                    frame.sequence,
                    frame.keyframe,
                    frame.captured_at_micros,
                    &frame.data,
                    |datagram| {
                        match connection.send_datagram(datagram.to_vec().into()) {
                            Ok(()) => {
                                stats.datagrams_sent.fetch_add(1, Ordering::Relaxed);
                                stats
                                    .bytes_sent
                                    .fetch_add(datagram.len() as u64, Ordering::Relaxed);
                            }
                            Err(e) => {
                                // Отказ отправки — это переполнение
                                // буфера QUIC, то есть сеть не
                                // успевает. Считаем как потерю: кадр
                                // всё равно не соберётся, а рвать
                                // сессию из-за перегрузки нельзя.
                                //
                                // Уровень `debug`, а не `trace`:
                                // потерянный фрагмент означает
                                // потерянный **кадр целиком**, и это
                                // надо видеть без пересборки с
                                // включённым trace.
                                stats.datagrams_dropped.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(?e, "датаграм не ушёл — кадр развалится");
                            }
                        }
                        Ok(())
                    },
                );

                match result {
                    Ok(_fragments) => {
                        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!(?e, seq = frame.sequence, "кадр не отправлен");
                    }
                }
            }
        }
    }

    stats.connected.store(false, Ordering::Relaxed);
    connection.close(0u32.into(), b"session ended");
    tracing::info!("поток транспорта завершён");
}

/// Дождаться кадра из синхронного канала, не блокируя рантайм.
///
/// `crossbeam` синхронен, а рантайм однопоточный: прямой `recv`
/// остановил бы и приём датаграмов. Короткий таймаут вместо
/// блокировки даёт циклу возможность проверять флаг остановки и
/// продолжать принимать.
async fn recv_async(outgoing: &Receiver<Outgoing>) -> PollOutgoing {
    match outgoing.try_recv() {
        Ok(frame) => PollOutgoing::Frame(Box::new(frame)),
        Err(TryRecvError::Empty) => {
            // Пауза короче кадра при 60 fps: задержки не добавляет,
            // а процессор не жжёт.
            tokio::time::sleep(Duration::from_micros(500)).await;
            PollOutgoing::Idle
        }
        Err(TryRecvError::Disconnected) => PollOutgoing::Closed,
    }
}

/// Что дал опрос очереди отправки.
///
/// Три состояния, а не `Option`. Первая версия возвращала `Option` и
/// отдавала `None` в двух разных смыслах — «пока пусто» и «канал
/// закрыт». Вызывающий различить их не мог и выходил из цикла в обоих
/// случаях: соединение устанавливалось и **тут же завершалось**, не
/// отправив ни кадра.
///
/// Урок общий: если функция отвечает «ничего» по двум разным причинам,
/// а реакция на них разная — `Option` здесь неверный тип.
enum PollOutgoing {
    /// Кадр готов к отправке.
    ///
    /// В боксе, потому что вариант заметно крупнее остальных, а
    /// значение живёт в `select!` на каждой итерации цикла.
    Frame(Box<Outgoing>),
    /// Очередь пуста — продолжаем работу.
    Idle,
    /// Канал закрыт: пайплайн завершился.
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_queue_refuses_instead_of_growing() {
        // Проверяется поведение, а не значение константы: очередь
        // обязана **отказать**, когда полна, а не расти. Растущая
        // очередь превращается в задержку, а устаревший кадр не
        // нужен никому (CLAUDE.md §5.3).
        //
        // Первая версия этого теста сравнивала константы между собой
        // (`SEND_QUEUE_FRAMES > 0`), то есть проверяла то, что видно
        // в коде и вычисляется компилятором. Clippy справедливо
        // назвал это утверждением с постоянным значением.
        let (tx, _rx) = crossbeam_channel::bounded::<u32>(SEND_QUEUE_FRAMES);

        for i in 0..SEND_QUEUE_FRAMES {
            assert!(tx.try_send(i as u32).is_ok(), "кадр {i} не принят");
        }

        assert!(
            matches!(tx.try_send(999), Err(TrySendError::Full(_))),
            "переполненная очередь обязана отказывать, а не расти"
        );
    }

    #[test]
    fn closed_channel_is_distinguishable_from_empty() {
        // Дефект, из-за которого соединение устанавливалось и тут же
        // завершалось: `recv_async` возвращала `None` и на пустой
        // очереди, и на закрытом канале, а вызывающий выходил из
        // цикла в обоих случаях.
        //
        // Здесь проверяется, что различить эти состояния можно —
        // именно на этом свойстве построен `PollOutgoing`.
        let (tx, rx) = crossbeam_channel::bounded::<u32>(1);

        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "пустая очередь"
        );

        drop(tx);
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Disconnected)),
            "закрытый канал обязан отличаться от пустого"
        );
    }

    #[test]
    fn waiting_returns_as_soon_as_a_frame_arrives() {
        // Свойство, ради которого существует `receive_timeout`: ждать
        // надо НА КАНАЛЕ, и просыпаться по приходу кадра, а не по
        // истечении таймаута.
        //
        // Без этого клиент забирал кадр на следующем такте своего
        // цикла, и на localhost это давало ~20 мс из 21 в стадии
        // `network` — там, где сети фактически нет (находка 41).
        //
        // Проверяется поведение канала, а не обёртки: `receive_timeout`
        // тонкая, а поднимать настоящее QUIC-соединение в юнит-тесте
        // значит проверять сеть, а не ожидание.
        let (tx, rx) = crossbeam_channel::bounded::<u32>(1);

        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let _ = tx.send(7);
        });

        let started = std::time::Instant::now();
        let got = rx.recv_timeout(Duration::from_secs(5));
        let waited = started.elapsed();
        sender.join().expect("поток отправителя упал");

        assert_eq!(got, Ok(7), "кадр обязан дойти");
        // Проснулись по кадру (~20 мс), а не досидели до таймаута (5 с).
        // Порог с большим запасом: тест не должен падать от загрузки
        // машины, но обязан падать, если ожидание идёт до таймаута.
        assert!(
            waited < Duration::from_secs(1),
            "ожидание длилось {waited:?}: проснулись не по кадру"
        );
    }

    #[test]
    fn waiting_gives_up_when_nothing_arrives() {
        // Обратное свойство: если кадра нет, ждать вечно нельзя.
        // Клиент обязан вернуться в цикл и перерисовать окно, иначе
        // Windows пометит его зависшим (находка 37) — а на статичном
        // экране хост честно ничего не шлёт.
        let (_tx, rx) = crossbeam_channel::bounded::<u32>(1);

        let started = std::time::Instant::now();
        let got = rx.recv_timeout(Duration::from_millis(30));
        assert!(got.is_err(), "пустой канал не должен ничего выдать");
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "вернулись раньше таймаута — ожидания не было"
        );
    }
}
