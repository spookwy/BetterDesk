//! Транспорт-заглушка: канал в памяти.
//!
//! # Почему заглушка, а не сразу QUIC
//!
//! Этап 1 отвечает на вопрос «укладывается ли пайплайн в бюджет»
//! (docs/roadmap.md). Настоящая сеть на этом шаге вредна: она добавит
//! свою задержку, свой джиттер и свой congestion control — и мы не
//! узнаем, сколько стоит наш собственный код. QUIC приходит на
//! этапе 3, когда цена всего остального уже измерена.
//!
//! # Что заглушка делает по-настоящему
//!
//! Всё, кроме собственно сети: нумерация кадров, нарезка на датаграмы
//! под MTU, сборка на приёме, счёт потерь. Благодаря этому этап 3
//! сводится к замене транспорта датаграмов, а не к появлению сразу
//! всей логики фрагментации поверх незнакомой сети.
//!
//! # Эмуляция плохого канала
//!
//! [`LinkProfile`] вносит потери и задержку. Это не имитация ради
//! красоты: без потерь ветка «неполный кадр» в [`Reassembler`] никогда
//! не исполнится, и мы получим ровно ту же ошибку, что с `recover()`
//! на этапе 1 — код есть, счётчик нулевой, работоспособность
//! неизвестна (CLAUDE.md §0.1, находка 26).

use crate::error::{Result, TransportError};
use crate::fragment::{Fragmenter, ReassembledFrame, Reassembler, ReceiveOutcome};
use crate::packet::PayloadKind;
use bd_core::metrics::{FrameTimings, Stage};
use bd_core::time::Epoch;
use std::collections::VecDeque;
use std::time::Duration;

/// Профиль эмулируемого канала.
///
/// Значения соответствуют профилям тестирования из CLAUDE.md §10.3.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkProfile {
    /// Доля теряемых датаграмов, от 0.0 до 1.0.
    pub loss: f64,
    /// Задержка доставки в одну сторону.
    ///
    /// Заглушка живёт в одном процессе, поэтому задержка не
    /// «прокручивается» ожиданием — датаграм просто удерживается
    /// в очереди указанное время. Спать в горячем пути нельзя:
    /// это остановило бы захват.
    pub delay: Duration,
}

impl LinkProfile {
    /// Идеальный канал: ни потерь, ни задержки.
    ///
    /// Профиль по умолчанию для замера чистой стоимости пайплайна.
    pub const PERFECT: Self = Self {
        loss: 0.0,
        delay: Duration::ZERO,
    };

    /// LAN: 0.1 % потерь, 1 мс.
    pub const LAN: Self = Self {
        loss: 0.001,
        delay: Duration::from_millis(1),
    };

    /// Хороший интернет: 0.5 % потерь, 30 мс.
    pub const INTERNET: Self = Self {
        loss: 0.005,
        delay: Duration::from_millis(30),
    };

    /// Плохой мобильный: 3 % потерь, 80 мс.
    pub const MOBILE: Self = Self {
        loss: 0.03,
        delay: Duration::from_millis(80),
    };

    /// Ровно 1 % потерь, 20 мс — профиль под критерий этапа 3.
    ///
    /// Заведён потому, что критерий сформулирован в конкретных
    /// цифрах: «потеря 1 % пакетов не вызывает видимых артефактов
    /// дольше 200 мс» (docs/roadmap.md). Проверять его на профиле
    /// с 0.5 % или 3 % — значит проверять не то, что записано, и
    /// получить вердикт, который ничего не доказывает (находка 29).
    pub const LOSSY: Self = Self {
        loss: 0.01,
        delay: Duration::from_millis(20),
    };
}

impl Default for LinkProfile {
    fn default() -> Self {
        Self::PERFECT
    }
}

/// Датаграм в пути.
#[derive(Debug)]
struct InFlight {
    /// Когда датаграм станет доступен приёмнику.
    due: bd_core::time::Timestamp,
    bytes: Vec<u8>,
}

/// Счётчики транспорта для оверлея.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransportStats {
    /// Отправлено кадров.
    pub frames_sent: u64,
    /// Доставлено кадров целиком.
    pub frames_received: u64,
    /// Кадров потеряно (не собрались).
    pub frames_lost: u64,
    /// Отправлено датаграмов.
    pub datagrams_sent: u64,
    /// Датаграмов потеряно каналом.
    pub datagrams_dropped: u64,
    /// Отправлено байтов полезной нагрузки.
    pub bytes_sent: u64,
}

impl TransportStats {
    /// Доля потерянных датаграмов, от 0.0 до 1.0.
    pub fn datagram_loss(&self) -> f64 {
        if self.datagrams_sent == 0 {
            return 0.0;
        }
        self.datagrams_dropped as f64 / self.datagrams_sent as f64
    }

    /// Битрейт в битах в секунду за указанный интервал.
    pub fn bitrate(&self, elapsed: Duration) -> f64 {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.bytes_sent as f64 * 8.0 / secs
    }
}

/// Канал в памяти между отправителем и приёмником.
///
/// Обе стороны живут в одном потоке пробы, поэтому синхронизации нет:
/// на этапе 3 её место займёт настоящий сокет, и добавлять здесь
/// мьютексы «на будущее» значило бы мерить не то, что нужно.
#[derive(Debug)]
pub struct LoopbackTransport {
    fragmenter: Fragmenter,
    reassembler: Reassembler,
    profile: LinkProfile,
    epoch: Epoch,
    in_flight: VecDeque<InFlight>,
    stats: TransportStats,
    /// Номер следующего кадра — свой у каждого вида нагрузки.
    ///
    /// Причина раздельности та же, что у QUIC: в одном канале виды
    /// с общим счётчиком видели бы дыры на месте чужих номеров и
    /// считали бы их потерями (см. `QuicTransport::next_sequence`).
    sequence: Vec<(PayloadKind, u64)>,
    /// Состояние генератора псевдослучайных чисел для потерь.
    ///
    /// Свой xorshift, а не `rand`: одна зависимость меньше, а качество
    /// случайности здесь роли не играет — нужна воспроизводимость.
    rng: u64,
}

impl LoopbackTransport {
    /// Канал с заданным профилем.
    ///
    /// `epoch` — та же, что у остального пайплайна, иначе отметки
    /// `Sent`/`Received` окажутся несравнимы с прочими (CLAUDE.md §4.5).
    pub fn new(profile: LinkProfile, epoch: Epoch) -> Self {
        Self {
            fragmenter: Fragmenter::new(),
            // Три кадра: при 60 fps это 50 мс терпения к
            // переупорядочиванию — больше, чем весь наш бюджет,
            // так что ограничение заведомо не мешает.
            reassembler: Reassembler::new(3),
            profile,
            epoch,
            in_flight: VecDeque::new(),
            stats: TransportStats::default(),
            sequence: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Текущие счётчики.
    pub fn stats(&self) -> TransportStats {
        self.stats
    }

    /// Профиль канала.
    pub fn profile(&self) -> LinkProfile {
        self.profile
    }

    /// Сменить профиль канала на лету.
    pub fn set_profile(&mut self, profile: LinkProfile) {
        self.profile = profile;
    }

    /// Отправить кадр.
    ///
    /// Транспорт не смотрит внутрь `data` (CLAUDE.md §4.2.4): для него
    /// это просто байты с приоритетом, заданным `kind`.
    ///
    /// `timings` получают отметку [`Stage::Sent`] и едут дальше внутри
    /// самого потока — так же, как будет с QUIC, где отметка попадает
    /// в заголовок кадра.
    pub fn send(
        &mut self,
        kind: PayloadKind,
        keyframe: bool,
        data: &[u8],
        timings: &mut FrameTimings,
    ) -> Result<u64> {
        let counter = match self.sequence.iter().position(|(k, _)| *k == kind) {
            Some(index) => index,
            None => {
                self.sequence.push((kind, 0));
                self.sequence.len() - 1
            }
        };
        let sequence = self.sequence[counter].1;
        self.sequence[counter].1 += 1;

        timings.mark(Stage::Sent, self.epoch.stamp_now());

        let now = self.epoch.stamp_now();
        let due = bd_core::time::Timestamp::from_micros(
            now.as_micros()
                .saturating_add(self.profile.delay.as_micros() as u64),
        );

        let mut dropped = 0u64;
        let mut sent = 0u64;
        let mut bytes = 0u64;

        // Колбэк не может занять `self` целиком (иначе не выйдет
        // одновременно писать в очередь и читать профиль), поэтому
        // потери считаются в локальные переменные.
        let profile_loss = self.profile.loss;
        let mut rng = self.rng;
        let in_flight = &mut self.in_flight;

        // Метка захвата берётся из таймингов кадра: она поставлена
        // ещё в `bd-capture`, и это единственное место, где известен
        // настоящий момент захвата (§4.5).
        //
        // Ноль означает «метки нет» — так бывает у ввода и курсора,
        // которые не проходят через захват экрана. Приёмник обязан
        // это учитывать и не считать по ним задержку.
        let captured_at = timings.get(Stage::Captured).map_or(0, |t| t.as_micros());

        self.fragmenter
            .fragment(kind, sequence, keyframe, captured_at, data, |datagram| {
                sent += 1;
                bytes += datagram.len() as u64;

                // xorshift64: воспроизводимо и достаточно равномерно
                // для эмуляции потерь.
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let roll = (rng >> 11) as f64 / (1u64 << 53) as f64;

                if roll < profile_loss {
                    dropped += 1;
                    return Ok(());
                }

                in_flight.push_back(InFlight {
                    due,
                    bytes: datagram.to_vec(),
                });
                Ok(())
            })?;

        self.rng = rng;
        self.stats.frames_sent += 1;
        self.stats.datagrams_sent += sent;
        self.stats.datagrams_dropped += dropped;
        self.stats.bytes_sent += bytes;

        Ok(sequence)
    }

    /// Забрать следующий готовый кадр, если он есть.
    ///
    /// Неблокирующий: пайплайн синхронный, и ожидание здесь остановило
    /// бы захват (CLAUDE.md §4.4, про выделенные потоки без async).
    ///
    /// Отметка [`Stage::Received`] ставится в момент, когда кадр собран
    /// целиком, — именно это и есть «кадр получен» с точки зрения
    /// декодера.
    pub fn receive(&mut self, timings: &mut FrameTimings) -> Result<Option<ReassembledFrame>> {
        let now = self.epoch.stamp_now();

        while let Some(front) = self.in_flight.front() {
            if front.due.as_micros() > now.as_micros() {
                // Очередь упорядочена по времени отправки, а задержка
                // постоянна — значит, дальше все ещё «в пути».
                break;
            }
            let Some(packet) = self.in_flight.pop_front() else {
                break;
            };

            match self.reassembler.accept(&packet.bytes) {
                Ok(ReceiveOutcome::Frame(frame)) => {
                    timings.mark(Stage::Received, self.epoch.stamp_now());
                    self.stats.frames_received += 1;
                    self.stats.frames_lost = self.reassembler.lost_frames();
                    return Ok(Some(*frame));
                }
                Ok(_) => continue,
                // Мусорный датаграм не рвёт сессию: он мог прийти
                // от кого угодно (§8.5).
                Err(err) if err.is_recoverable() => continue,
                Err(err) => return Err(err),
            }
        }

        self.stats.frames_lost = self.reassembler.lost_frames();
        Ok(None)
    }

    /// Сколько датаграмов сейчас в пути.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Отбросить всё, что в пути.
    ///
    /// Нужно при пересоздании кодеков: старые байты после смены
    /// разрешения декодировать нечем.
    pub fn flush(&mut self) {
        self.in_flight.clear();
    }
}

/// Проверка, что размер кадра осмыслен до отправки.
///
/// Вынесено отдельно, чтобы вызывающий мог решить сам: дропнуть кадр
/// или запросить у энкодера меньший битрейт.
pub fn frame_fits(fragmenter: &Fragmenter, bytes: usize) -> Result<()> {
    let count = fragmenter.fragment_count(bytes);
    if count > crate::fragment::MAX_FRAGMENTS {
        return Err(TransportError::TooLarge {
            bytes,
            limit: crate::fragment::MAX_FRAGMENTS,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(profile: LinkProfile) -> LoopbackTransport {
        LoopbackTransport::new(profile, Epoch::new())
    }

    #[test]
    fn lossy_profile_matches_the_stage_criterion() {
        // Критерий этапа 3 сформулирован в конкретных цифрах —
        // «потеря 1 %» — и профиль обязан им соответствовать.
        // Проверять критерий на канале с другими потерями значило бы
        // выносить вердикт не о том, что записано (находка 29).
        assert!(
            (LinkProfile::LOSSY.loss - 0.01).abs() < f64::EPSILON,
            "профиль lossy обязан давать ровно 1 % потерь"
        );
        // Дальше проверяется ПОВЕДЕНИЕ, а не сравнение констант между
        // собой: последнее вычисляет компилятор, и такой тест не
        // доказывает ничего (тот же урок, что находка 38).
        //
        // Канал обязан реально терять датаграмы — иначе критерий
        // проверялся бы на канале, который ничего не теряет, и
        // зелёный результат означал бы лишь, что эмуляция сломана.
        let mut t = transport(LinkProfile::LOSSY);
        let payload: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        for _ in 0..60 {
            let mut timings = FrameTimings::default();
            let _ = t.send(PayloadKind::Video, false, &payload, &mut timings);
            let mut arrival = FrameTimings::default();
            while t.receive(&mut arrival).ok().flatten().is_some() {}
        }
        assert!(
            t.stats().datagrams_dropped > 0,
            "профиль lossy обязан терять датаграмы, потеряно 0"
        );
    }

    #[test]
    fn perfect_link_delivers_every_frame_intact() {
        let mut t = transport(LinkProfile::PERFECT);
        let payload: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();

        for _ in 0..10 {
            let mut timings = FrameTimings::new(0);
            t.send(PayloadKind::Video, false, &payload, &mut timings)
                .expect("отправка");
            let frame = t
                .receive(&mut timings)
                .expect("приём")
                .expect("кадр обязан дойти по идеальному каналу");
            assert_eq!(frame.data, payload);
        }

        let stats = t.stats();
        assert_eq!(stats.frames_sent, 10);
        assert_eq!(stats.frames_received, 10);
        assert_eq!(stats.frames_lost, 0);
        assert_eq!(stats.datagrams_dropped, 0);
    }

    #[test]
    fn send_and_receive_mark_their_stages() {
        // Без этих отметок в оверлее не появится строка «network»,
        // а разбивка перестанет объяснять итог (§0.1, находка 24).
        let mut t = transport(LinkProfile::PERFECT);
        let mut timings = FrameTimings::new(1);
        timings.mark(Stage::Captured, Epoch::new().stamp_now());

        t.send(PayloadKind::Video, true, b"payload", &mut timings)
            .expect("отправка");
        assert!(timings.get(Stage::Sent).is_some());

        t.receive(&mut timings).expect("приём").expect("кадр");
        assert!(timings.get(Stage::Received).is_some());
    }

    #[test]
    fn sequence_numbers_increase_monotonically() {
        let mut t = transport(LinkProfile::PERFECT);
        let mut timings = FrameTimings::new(0);
        let first = t
            .send(PayloadKind::Video, false, b"a", &mut timings)
            .expect("отправка");
        let second = t
            .send(PayloadKind::Video, false, b"b", &mut timings)
            .expect("отправка");
        assert_eq!(second, first + 1);
    }

    #[test]
    fn one_channel_carries_three_kinds_without_phantom_losses() {
        // Сквозная проверка сведения каналов в одно соединение
        // (было три порта — находка 56). Через один транспорт идут
        // видео, ввод и курсор вперемешку; дойти обязаны все, а
        // счётчик потерь обязан остаться нулевым.
        //
        // Ноль здесь — не украшение отчёта: потеря видеокадра
        // означает запрос ключевого (находка 52), и ложные потери
        // на исправном канале дали бы шторм ключевых кадров.
        let mut t = transport(LinkProfile::PERFECT);
        let mut timings = FrameTimings::new(0);

        const ROUNDS: usize = 20;
        let kinds = [PayloadKind::Video, PayloadKind::Input, PayloadKind::Control];
        for _ in 0..ROUNDS {
            for kind in kinds {
                t.send(kind, false, b"payload", &mut timings)
                    .expect("отправка");
            }
        }

        let mut delivered = [0usize; 3];
        while let Some(frame) = t.receive(&mut timings).expect("приём") {
            let index = kinds
                .iter()
                .position(|k| *k == frame.kind)
                .expect("вид из числа отправленных");
            delivered[index] += 1;
        }

        for (index, kind) in kinds.iter().enumerate() {
            assert_eq!(
                delivered[index], ROUNDS,
                "вид {kind:?} дошёл не целиком: {} из {ROUNDS}",
                delivered[index]
            );
        }
        assert_eq!(
            t.stats().frames_lost,
            0,
            "исправный канал не должен показывать потерь"
        );
    }

    #[test]
    fn each_kind_numbers_its_frames_from_zero() {
        // Нумерация раздельная, поэтому первый кадр каждого вида
        // получает номер 0. Общий счётчик выдал бы 0, 1, 2 — и у
        // приёмника два вида из трёх начались бы с дыры.
        let mut t = transport(LinkProfile::PERFECT);
        let mut timings = FrameTimings::new(0);
        for kind in [PayloadKind::Video, PayloadKind::Input, PayloadKind::Control] {
            let first = t.send(kind, false, b"x", &mut timings).expect("отправка");
            assert_eq!(first, 0, "первый кадр вида {kind:?} обязан быть нулевым");
        }
    }

    #[test]
    fn lossy_link_actually_loses_frames() {
        // Проверка самой проверки: если эмуляция потерь не работает,
        // ветка «неполный кадр» в сборщике никогда не исполнится, и мы
        // повторим находку 26 — код есть, но не проверен.
        let mut t = transport(LinkProfile {
            loss: 0.2,
            delay: Duration::ZERO,
        });
        // Кадр в несколько фрагментов: при 20 % потерь на датаграм
        // шанс уцелеть целиком мал.
        let payload = vec![0u8; 20_000];

        let mut delivered = 0;
        for _ in 0..100 {
            let mut timings = FrameTimings::new(0);
            t.send(PayloadKind::Video, false, &payload, &mut timings)
                .expect("отправка");
            while let Some(_frame) = t.receive(&mut timings).expect("приём") {
                delivered += 1;
            }
        }

        let stats = t.stats();
        assert!(stats.datagrams_dropped > 0, "канал обязан терять датаграмы");
        assert!(
            delivered < 100,
            "при 20 % потерь не все кадры могут дойти, дошло {delivered}"
        );
        assert!(
            stats.frames_lost > 0,
            "потерянные кадры обязаны попасть в счётчик"
        );
    }

    #[test]
    fn perfect_link_after_lossy_recovers_completely() {
        // Потери не должны оставлять сборщик в сломанном состоянии.
        let mut t = transport(LinkProfile {
            loss: 0.5,
            delay: Duration::ZERO,
        });
        let payload = vec![7u8; 10_000];
        for _ in 0..20 {
            let mut timings = FrameTimings::new(0);
            t.send(PayloadKind::Video, false, &payload, &mut timings)
                .expect("отправка");
            while t.receive(&mut timings).expect("приём").is_some() {}
        }

        t.set_profile(LinkProfile::PERFECT);
        let mut delivered = 0;
        for _ in 0..10 {
            let mut timings = FrameTimings::new(0);
            t.send(PayloadKind::Video, false, &payload, &mut timings)
                .expect("отправка");
            while let Some(frame) = t.receive(&mut timings).expect("приём") {
                assert_eq!(frame.data, payload, "данные обязаны быть целыми");
                delivered += 1;
            }
        }
        assert_eq!(delivered, 10, "после потерь канал обязан снова работать");
    }

    #[test]
    fn delayed_link_holds_frames_until_due() {
        let mut t = transport(LinkProfile {
            loss: 0.0,
            delay: Duration::from_millis(50),
        });
        let mut timings = FrameTimings::new(0);
        t.send(PayloadKind::Video, false, b"data", &mut timings)
            .expect("отправка");

        assert!(
            t.receive(&mut timings).expect("приём").is_none(),
            "кадр не должен приходить раньше срока"
        );
        assert_eq!(t.in_flight(), 1);

        std::thread::sleep(Duration::from_millis(60));
        assert!(
            t.receive(&mut timings).expect("приём").is_some(),
            "после задержки кадр обязан прийти"
        );
    }

    #[test]
    fn flush_discards_frames_in_flight() {
        let mut t = transport(LinkProfile {
            loss: 0.0,
            delay: Duration::from_secs(10),
        });
        let mut timings = FrameTimings::new(0);
        t.send(PayloadKind::Video, false, b"data", &mut timings)
            .expect("отправка");
        assert_eq!(t.in_flight(), 1);
        t.flush();
        assert_eq!(t.in_flight(), 0);
    }

    #[test]
    fn stats_report_loss_and_bitrate() {
        let stats = TransportStats {
            datagrams_sent: 1000,
            datagrams_dropped: 25,
            bytes_sent: 1_000_000,
            ..Default::default()
        };
        assert!((stats.datagram_loss() - 0.025).abs() < 1e-9);
        assert!((stats.bitrate(Duration::from_secs(1)) - 8_000_000.0).abs() < 1.0);
        // Нулевой интервал не должен давать деления на ноль.
        assert_eq!(stats.bitrate(Duration::ZERO), 0.0);
        assert_eq!(TransportStats::default().datagram_loss(), 0.0);
    }

    #[test]
    fn delayed_link_never_delivers_in_the_sending_iteration() {
        // Диагностика расхождения, найденного 30-минутным прогоном:
        // профиль `lan` объявляет 1 мс задержки, а стадия `network`
        // в отчёте показывала ~20 мс.
        //
        // Транспорт здесь ни при чём — дело в том, КАК его опрашивают.
        // При ненулевой задержке кадр физически не может быть готов
        // в той же итерации, где отправлен. Вызывающий, делающий один
        // `receive()` на один `send()`, забирает не свежий кадр, а
        // предыдущий, и разница `Sent → Received` начинает мерить шаг
        // цикла кодирования (16.7 мс при 60 fps), а не канал.
        //
        // Тест фиксирует первое звено этой цепочки: доставки в своей
        // итерации не бывает. Отсюда и 20 мс в отчёте — 1 мс канала
        // плюс ожидание следующей итерации.
        let mut t = transport(LinkProfile {
            loss: 0.0,
            delay: Duration::from_millis(1),
        });

        let payload = vec![7u8; 8000];
        let mut same_iteration = 0;

        for _ in 0..20 {
            let mut timings = FrameTimings::new(0);
            t.send(PayloadKind::Video, false, &payload, &mut timings)
                .expect("отправка");

            let mut arrival = FrameTimings::new(0);
            if t.receive(&mut arrival).expect("приём").is_some() {
                same_iteration += 1;
            }
        }

        assert_eq!(
            same_iteration, 0,
            "кадр не может дойти раньше, чем истечёт задержка канала"
        );
    }
}
