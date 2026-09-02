//! Контроллер битрейта: реакция на состояние канала.
//!
//! # Задача
//!
//! FEC (этап 3) спасает кадры от потерь, но избыточность и битрейт
//! задаются флагом и не меняются, что бы ни происходило. Через
//! интернет это неверно вдвойне: канал меняется в течение сессии, а
//! запас, заданный при запуске, либо тратится впустую, либо
//! кончается.
//!
//! # Почему сигнал — задержка, а не потери
//!
//! Потери — **поздний** признак. К моменту, когда датаграмы начали
//! теряться, очередь у роутера уже переполнена, и в ней стоят наши
//! же кадры: задержка выросла раньше, чем появилась первая потеря.
//! Снижать битрейт по потерям — значит реагировать, когда картинка
//! уже развалилась.
//!
//! Задержка доставки растёт **до** переполнения: очередь копится, и
//! каждый следующий кадр ждёт дольше предыдущего. Это принцип Google
//! Congestion Control, и он же записан в §5.3 CLAUDE.md как решение
//! для этапа 6.
//!
//! Практически это видно по RTT: у QUIC он измеряется постоянно, и
//! рост RTT над своим же минимумом означает, что пакеты стоят в
//! очереди, а не летят.
//!
//! # Почему минимум, а не среднее
//!
//! Минимальный наблюдённый RTT — это оценка **чистого времени
//! полёта**: меньше него доставка невозможна физически. Всё, что
//! сверх, — время в очередях. Среднее для этого не годится: оно само
//! растёт вместе с очередью и скрывает ровно то, что мы измеряем.
//!
//! Тот же приём, что в [`crate::clocksync`], и та же оговорка: окно
//! скользящее, иначе одно старое наблюдение навсегда определит
//! базовую линию, а маршруты между машинами меняются.
//!
//! # Чего этот контроллер НЕ делает
//!
//! Не меняет разрешение. Снижение битрейта до какого-то предела
//! перестаёт помогать: кадр 1080p при 1 Мбит/с превращается в кашу,
//! и правильный ответ — уменьшить картинку, а не сжимать сильнее.
//! Это отдельная работа: смена разрешения требует пересоздания
//! энкодера и декодера (находка 31), то есть паузы в потоке.

use core::time::Duration;

/// Решение контроллера — что делать с настройками отправки.
///
/// # Почему структура, а не «новый битрейт»
///
/// Битрейт и избыточность связаны: на плохом канале осмысленно и
/// снизить поток, и добавить паритета — иначе освободившуюся полосу
/// займут потери. Отдавать их порознь значило бы принимать два
/// решения по одному наблюдению, рискуя рассогласовать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Целевой битрейт видео, бит/с.
    pub bitrate: u32,
    /// Избыточность FEC в процентах; ноль — выключено.
    pub fec_percent: u32,
}

/// Как контроллер оценивает канал прямо сейчас.
///
/// Отдаётся наружу для отчёта и оверлея: человеку, который смотрит
/// на рваную картинку, важно видеть, что программа это заметила.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Канал держит поток: задержка у базовой линии, потерь нет.
    Good,
    /// Задержка растёт — очередь копится. Битрейт снижается **до**
    /// того, как появятся потери.
    Congested,
    /// Потери идут. Битрейт снижается, избыточность растёт.
    Lossy,
}

/// Настройки контроллера.
#[derive(Debug, Clone, Copy)]
pub struct RateConfig {
    /// Битрейт, с которого начинаем и выше которого не поднимаемся.
    ///
    /// Это то, что задал человек (`--bitrate`) или посчитала формула
    /// от разрешения. Контроллер только опускается от него и
    /// возвращается обратно: сам решить, что канал вытянет больше
    /// заданного, он не вправе.
    pub max_bitrate: u32,

    /// Нижняя граница. Ниже неё картинка бесполезна, и правильный
    /// ответ — менять разрешение, а не сжимать сильнее.
    pub min_bitrate: u32,

    /// На сколько снижать за один шаг (доля от текущего).
    ///
    /// Снижение резче подъёма — намеренная асимметрия, см.
    /// [`Self::rise_step`].
    pub fall_step: f32,

    /// На сколько повышать за один шаг (доля от текущего).
    ///
    /// **Поднимаемся медленнее, чем опускаемся**, и это не
    /// осторожность ради осторожности. Ошибка вниз стоит качества
    /// картинки на секунду; ошибка вверх переполняет очередь и стоит
    /// потерянных кадров плюс времени на восстановление. Цена
    /// несимметрична — реакция тоже.
    pub rise_step: f32,

    /// Насколько RTT должен превысить базовую линию, чтобы считать
    /// канал перегруженным.
    ///
    /// Порог в долях: 1.5 означает «RTT в полтора раза выше
    /// минимального». Доля, а не абсолютные миллисекунды, потому что
    /// база разная: в LAN 0.2 мс, через интернет 70 мс, и +20 мс
    /// значат в этих случаях совершенно разное.
    ///
    /// **Одной доли недостаточно**, см. [`Self::congestion_floor`].
    pub congestion_ratio: f32,

    /// Минимальный абсолютный рост задержки, ниже которого перегрузка
    /// не объявляется, каким бы ни было отношение.
    ///
    /// # Зачем, если есть доля
    ///
    /// Живой прогон на localhost показал, зачем. База там 0.2 мс,
    /// то есть порог по доле — 0.3 мс, и его превышает **джиттер
    /// планировщика Windows**: обычные 1.8 мс выглядят как рост в
    /// девять раз. Контроллер объявил перегрузку на заведомо
    /// исправном канале и за двадцать секунд свёл битрейт с 15 до
    /// 1 Мбит/с.
    ///
    /// Относительный порог верен по смыслу, но при крошечной базе
    /// теряет смысл: отношение измеряет не очередь, а шум. Абсолютный
    /// порог отсекает именно этот случай, не мешая работать там, где
    /// база велика: через интернет 5 мс — это действительно очередь.
    pub congestion_floor: Duration,

    /// Доля потерянных датаграмов, при которой канал считается
    /// теряющим.
    pub loss_threshold: f32,

    /// Как часто принимать решение.
    ///
    /// Не чаще, чем канал успевает отреагировать: снизив битрейт,
    /// надо дать очереди рассосаться, иначе следующий замер покажет
    /// прежнюю задержку и контроллер снизит ещё раз — и так до дна.
    pub interval: Duration,
}

impl RateConfig {
    /// Настройки по умолчанию для заданного потолка битрейта.
    pub fn new(max_bitrate: u32) -> Self {
        Self {
            max_bitrate,
            // Ниже мегабита 1080p не имеет смысла — см. шапку.
            min_bitrate: 1_000_000,
            // Вниз на четверть: заметно, но не обрушивает качество.
            fall_step: 0.25,
            // Вверх на восьмую: вдвое медленнее, чем вниз.
            rise_step: 0.125,
            congestion_ratio: 1.5,
            // Пять миллисекунд: через интернет это уже очередь, а
            // джиттер планировщика на localhost (единицы мс) сюда не
            // дотягивается.
            congestion_floor: Duration::from_millis(5),
            loss_threshold: 0.02,
            // Полсекунды: достаточно, чтобы очередь отреагировала на
            // прошлое решение, и достаточно быстро, чтобы человек не
            // успел устать от рваной картинки.
            interval: Duration::from_millis(500),
        }
    }
}

/// Наблюдение за каналом на момент решения.
#[derive(Debug, Clone, Copy)]
pub struct LinkSample {
    /// Круговое время, как его измеряет QUIC.
    pub rtt: Duration,
    /// Сколько датаграмов отправлено всего.
    pub datagrams_sent: u64,
    /// Сколько отброшено (очередь переполнена или сеть отказала).
    pub datagrams_dropped: u64,
}

/// Сколько наблюдений RTT держим для базовой линии.
///
/// 240 при решении раз в полсекунды — это две минуты. Достаточно,
/// чтобы пережить смену маршрута, и мало, чтобы не тащить за собой
/// вчерашнюю сеть.
const BASELINE_WINDOW: usize = 240;

/// Контроллер битрейта.
pub struct RateController {
    config: RateConfig,
    bitrate: u32,
    fec_percent: u32,
    state: LinkState,
    /// Скользящее окно RTT для оценки базовой линии.
    rtt_window: heapless_ring::Ring<u64, BASELINE_WINDOW>,
    /// Счётчики на момент прошлого решения — чтобы считать **разницу**,
    /// а не долю за всё время.
    ///
    /// Доля за всю сессию сглаживает свежие потери до незаметности:
    /// после часа чистой работы минута обрыва даёт доли процента.
    last_sent: u64,
    last_dropped: u64,
    /// Доля потерь на момент последнего снижения битрейта.
    ///
    /// Нужна, чтобы понять, помогает ли снижение вообще: потери от
    /// самого канала (Wi-Fi, мобильная сеть) от нашей нагрузки не
    /// зависят, и снижать битрейт против них бесполезно.
    loss_before_last_drop: Option<f32>,
    /// Когда принимали решение в прошлый раз, в микросекундах эпохи.
    last_decision_micros: Option<u64>,
}

impl RateController {
    /// Создать контроллер с заданными настройками.
    pub fn new(config: RateConfig) -> Self {
        Self {
            bitrate: config.max_bitrate,
            config,
            fec_percent: 0,
            state: LinkState::Good,
            rtt_window: heapless_ring::Ring::new(),
            last_sent: 0,
            last_dropped: 0,
            loss_before_last_drop: None,
            last_decision_micros: None,
        }
    }

    /// Текущее решение без нового наблюдения.
    pub fn current(&self) -> RateDecision {
        RateDecision {
            bitrate: self.bitrate,
            fec_percent: self.fec_percent,
        }
    }

    /// Как контроллер оценивает канал.
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// Базовая линия RTT — оценка чистого времени полёта.
    ///
    /// `None`, пока наблюдений нет.
    pub fn baseline_rtt(&self) -> Option<Duration> {
        self.rtt_window.min().map(Duration::from_micros)
    }

    /// Учесть наблюдение и, если пришло время, принять решение.
    ///
    /// `None` означает «рано»: интервал ещё не истёк. Наблюдение при
    /// этом всё равно учитывается — базовая линия набирается
    /// непрерывно, а не только в момент решения.
    ///
    /// `now_micros` — время из [`crate::time`], а не из `Instant`:
    /// контроллер должен быть проверяем в тестах без ожидания
    /// реального времени.
    pub fn observe(&mut self, sample: LinkSample, now_micros: u64) -> Option<RateDecision> {
        let rtt_micros = sample.rtt.as_micros() as u64;
        if rtt_micros > 0 {
            self.rtt_window.push(rtt_micros);
        }

        let due = match self.last_decision_micros {
            None => true,
            Some(previous) => {
                now_micros.saturating_sub(previous) >= self.config.interval.as_micros() as u64
            }
        };
        if !due {
            return None;
        }
        self.last_decision_micros = Some(now_micros);

        // Потери — за прошедший интервал, а не за всю сессию.
        let sent = sample.datagrams_sent.saturating_sub(self.last_sent);
        let dropped = sample.datagrams_dropped.saturating_sub(self.last_dropped);
        self.last_sent = sample.datagrams_sent;
        self.last_dropped = sample.datagrams_dropped;

        let loss = if sent == 0 {
            0.0
        } else {
            dropped as f32 / sent as f32
        };

        // Перегрузка — по превышению RTT над базовой линией.
        //
        // Базовая линия берётся по минимуму окна, но **не включая
        // само текущее наблюдение** как эталон: если канал стабильно
        // плох, минимум уедет вслед за ним, и рост станет незаметен.
        // Скользящее окно решает это тем, что старые (хорошие)
        // наблюдения из него уходят не сразу.
        // Перегрузка объявляется, только если рост значим **и в
        // долях, и в абсолютных величинах**.
        //
        // Одной доли мало: при базе в 0.2 мс (localhost) порог
        // составляет 0.3 мс, и его перекрывает джиттер планировщика.
        // Живой прогон показал ровно это — контроллер свёл битрейт с
        // 15 до 1 Мбит/с на заведомо исправном канале.
        //
        // Одного абсолютного порога тоже мало: через интернет с базой
        // 70 мс рост на 5 мс — это шум, а не очередь.
        let congested = match self.rtt_window.min() {
            Some(baseline) if baseline > 0 && rtt_micros > 0 => {
                let excess = rtt_micros.saturating_sub(baseline);
                let ratio_exceeded =
                    rtt_micros as f32 > baseline as f32 * self.config.congestion_ratio;
                let floor_exceeded = excess >= self.config.congestion_floor.as_micros() as u64;

                ratio_exceeded && floor_exceeded
            }
            _ => false,
        };

        self.state = if loss > self.config.loss_threshold {
            LinkState::Lossy
        } else if congested {
            LinkState::Congested
        } else {
            LinkState::Good
        };

        match self.state {
            LinkState::Lossy => {
                // Паритет — первый ответ на потери: 25 % покрывает
                // 3 % потерь датаграмов (замер этапа 3, находка 66).
                self.fec_percent = 25;

                // **Снижаем битрейт только пока это помогает.**
                //
                // Потери бывают двух родов, и лечатся они по-разному:
                //
                // - от перегрузки: очередь переполнена нашим же
                //   потоком, и снижение битрейта её разгружает;
                // - от самого канала (Wi-Fi, мобильная сеть, плохой
                //   кабель): доля потерь от нашей нагрузки почти не
                //   зависит, и снижать битрейт бессмысленно.
                //
                // Различить их можно по тому, **уменьшились ли потери
                // после прошлого снижения**. Если нет — снижение не
                // работает, и продолжать значит ухудшать картинку
                // задаром.
                //
                // Проба поймала ровно это: на профиле «3 % потерь»
                // первая версия за десять шагов свела битрейт с 15 до
                // 1 Мбит/с и там осталась. Потери при этом не
                // изменились — они и не могли, — а картинка при
                // 1 Мбит/с на 1080p бесполезна. Критерий этапа 6
                // («сессия остаётся пригодной для работы») был бы
                // провален ровно тем механизмом, который писался ради
                // него.
                let helping = match self.loss_before_last_drop {
                    // Первое снижение при этих потерях — пробуем.
                    None => true,
                    // Потери заметно упали — снижение работает.
                    Some(before) => loss < before * 0.75,
                };

                if helping {
                    self.loss_before_last_drop = Some(loss);
                    self.bitrate = self.scaled_down();
                }
            }
            LinkState::Congested => {
                self.bitrate = self.scaled_down();
                // Избыточность НЕ добавляем: очередь и так переполнена,
                // а паритет — это дополнительные датаграмы. Лечить
                // перегрузку добавлением трафика значит усугублять её.
                self.fec_percent = 0;
            }
            LinkState::Good => {
                // Канал выправился — забываем, что снижение не
                // помогало. В следующий раз потери могут быть уже
                // другого рода (очередь, а не помехи), и отказываться
                // пробовать из-за прошлого опыта нельзя.
                self.loss_before_last_drop = None;
                self.bitrate = self.scaled_up();
                // Паритет снимаем не сразу, а вместе с возвратом к
                // потолку: канал только что был плох, и убрать защиту
                // на первом же хорошем замере — напрашиваться на
                // повтор.
                if self.bitrate >= self.config.max_bitrate {
                    self.fec_percent = 0;
                }
            }
        }

        Some(self.current())
    }

    fn scaled_down(&self) -> u32 {
        let target = self.bitrate as f32 * (1.0 - self.config.fall_step);
        (target as u32).max(self.config.min_bitrate)
    }

    fn scaled_up(&self) -> u32 {
        let target = self.bitrate as f32 * (1.0 + self.config.rise_step);
        (target as u32).min(self.config.max_bitrate)
    }
}

/// Кольцевой буфер фиксированного размера без аллокаций.
///
/// Свой, а не из крейта: нужен один тип с одной операцией, а
/// зависимость пришлось бы проверять по §9.1 и тащить в `bd-core`,
/// который обязан оставаться лёгким и переносимым.
mod heapless_ring {
    /// Кольцо на `N` элементов; при переполнении вытесняет старейший.
    pub struct Ring<T, const N: usize> {
        items: [T; N],
        len: usize,
        next: usize,
    }

    impl<const N: usize> Ring<u64, N> {
        pub fn new() -> Self {
            Self {
                items: [0; N],
                len: 0,
                next: 0,
            }
        }

        pub fn push(&mut self, value: u64) {
            self.items[self.next] = value;
            self.next = (self.next + 1) % N;
            if self.len < N {
                self.len += 1;
            }
        }

        pub fn min(&self) -> Option<u64> {
            self.items[..self.len].iter().copied().min()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u32 = 15_000_000;

    fn controller() -> RateController {
        RateController::new(RateConfig::new(MAX))
    }

    /// Спокойный канал: RTT ровный, потерь нет.
    fn calm(rtt_ms: u64, sent: u64) -> LinkSample {
        LinkSample {
            rtt: Duration::from_millis(rtt_ms),
            datagrams_sent: sent,
            datagrams_dropped: 0,
        }
    }

    #[test]
    fn starts_at_the_ceiling() {
        assert_eq!(controller().current().bitrate, MAX);
    }

    /// Решения не чаще, чем задано интервалом: иначе контроллер
    /// снижал бы битрейт быстрее, чем канал успевает отреагировать.
    #[test]
    fn decisions_respect_the_interval() {
        let mut c = controller();

        assert!(c.observe(calm(20, 100), 0).is_some(), "первое решение");
        assert!(c.observe(calm(20, 200), 100_000).is_none(), "рано");
        assert!(c.observe(calm(20, 300), 400_000).is_none(), "всё ещё рано");
        assert!(c.observe(calm(20, 400), 500_000).is_some(), "пора");
    }

    /// **Главное свойство: рост задержки снижает битрейт ДО потерь.**
    ///
    /// Потерь в наблюдениях нет вовсе — реагировать контроллер обязан
    /// на одно только время доставки.
    #[test]
    fn rising_latency_lowers_bitrate_before_any_loss() {
        let mut c = controller();
        let mut now = 0u64;

        // Устанавливаем базовую линию: 20 мс.
        for i in 0..10 {
            c.observe(calm(20, i * 100), now);
            now += 500_000;
        }
        let before = c.current().bitrate;

        // Задержка выросла втрое, потерь по-прежнему НЕТ.
        let decision = c.observe(calm(60, 2000), now).expect("решение");

        assert_eq!(c.state(), LinkState::Congested);
        assert!(
            decision.bitrate < before,
            "битрейт не снижен: {} → {}",
            before,
            decision.bitrate
        );
    }

    /// **Шум при крошечной базе не считается перегрузкой.**
    ///
    /// Тест воспроизводит дефект, найденный живым прогоном на
    /// localhost: база 0.2 мс, наблюдения 1–2 мс. По отношению это
    /// рост в девять раз, по существу — джиттер планировщика.
    ///
    /// Первая версия контроллера объявляла здесь перегрузку и за
    /// двадцать секунд сводила битрейт с 15 до 1 Мбит/с на заведомо
    /// исправном канале.
    ///
    /// Проверен на способность провалиться: без `congestion_floor`
    /// падает.
    #[test]
    fn scheduler_jitter_is_not_congestion() {
        let mut c = controller();
        let mut now = 0u64;

        // База: 200 микросекунд, как на localhost.
        for i in 0..10 {
            c.observe(
                LinkSample {
                    rtt: Duration::from_micros(200),
                    datagrams_sent: i * 100,
                    datagrams_dropped: 0,
                },
                now,
            );
            now += 500_000;
        }

        // Обычный джиттер: 1.8 мс вместо 0.2. Отношение — девять раз.
        for i in 0..10 {
            let decision = c
                .observe(
                    LinkSample {
                        rtt: Duration::from_micros(1_800),
                        datagrams_sent: 1000 + i * 100,
                        datagrams_dropped: 0,
                    },
                    now,
                )
                .expect("решение");

            assert_eq!(
                c.state(),
                LinkState::Good,
                "джиттер планировщика принят за перегрузку"
            );
            assert_eq!(decision.bitrate, MAX, "битрейт снижен из-за шума");
            now += 500_000;
        }
    }

    /// А вот при большой базе тот же относительный рост — настоящая
    /// очередь, и реагировать надо.
    ///
    /// Пара к предыдущему тесту: без неё «не реагировать никогда»
    /// прошло бы оба.
    #[test]
    fn real_queueing_at_internet_latency_is_detected() {
        let mut c = controller();
        let mut now = 0u64;

        // База интернета: 70 мс.
        for i in 0..10 {
            c.observe(calm(70, i * 100), now);
            now += 500_000;
        }

        // Рост до 140 мс: вдвое, и +70 мс абсолютно — это очередь.
        let decision = c.observe(calm(140, 2000), now).expect("решение");

        assert_eq!(c.state(), LinkState::Congested);
        assert!(decision.bitrate < MAX);
    }

    /// Проверка на способность провалиться (находка 4): на ровном
    /// канале снижения быть не должно. Без этого предыдущий тест
    /// прошёл бы и у контроллера, который снижает битрейт всегда.
    #[test]
    fn steady_link_is_not_throttled() {
        let mut c = controller();
        let mut now = 0u64;

        for i in 0..20 {
            let decision = c.observe(calm(20, i * 100), now).expect("решение");
            assert_eq!(decision.bitrate, MAX, "битрейт снижен на ровном канале");
            assert_eq!(c.state(), LinkState::Good);
            now += 500_000;
        }
    }

    /// Потери включают избыточность и снижают битрейт.
    #[test]
    fn losses_enable_parity() {
        let mut c = controller();
        let mut now = 0u64;

        c.observe(calm(20, 1000), now);
        now += 500_000;

        // 5 % потерь за интервал — выше порога.
        let decision = c
            .observe(
                LinkSample {
                    rtt: Duration::from_millis(20),
                    datagrams_sent: 2000,
                    datagrams_dropped: 50,
                },
                now,
            )
            .expect("решение");

        assert_eq!(c.state(), LinkState::Lossy);
        assert!(decision.fec_percent > 0, "избыточность не включена");
        assert!(decision.bitrate < MAX);
    }

    /// **Перегрузка НЕ добавляет избыточность.**
    ///
    /// Паритет — это лишние датаграмы, а очередь и так переполнена.
    /// Лечить перегрузку добавлением трафика — усугублять её.
    #[test]
    fn congestion_does_not_add_traffic() {
        let mut c = controller();
        let mut now = 0u64;

        for i in 0..10 {
            c.observe(calm(20, i * 100), now);
            now += 500_000;
        }
        let decision = c.observe(calm(80, 2000), now).expect("решение");

        assert_eq!(c.state(), LinkState::Congested);
        assert_eq!(decision.fec_percent, 0, "избыточность добавлена в затор");
    }

    /// Восстановление медленнее падения — асимметрия намеренная.
    #[test]
    fn recovery_is_slower_than_the_fall() {
        let mut c = controller();
        let mut now = 0u64;

        for i in 0..10 {
            c.observe(calm(20, i * 100), now);
            now += 500_000;
        }

        let before_fall = c.current().bitrate;
        let after_fall = c.observe(calm(90, 2000), now).expect("падение").bitrate;
        now += 500_000;
        let after_rise = c.observe(calm(20, 2100), now).expect("подъём").bitrate;

        let fall = before_fall - after_fall;
        let rise = after_rise - after_fall;
        assert!(rise < fall, "подъём ({rise}) не медленнее падения ({fall})");
    }

    /// Битрейт не уходит ниже дна: там картинка бесполезна, и
    /// правильный ответ — менять разрешение, а не сжимать сильнее.
    #[test]
    fn bitrate_never_goes_below_the_floor() {
        let mut c = controller();
        let mut now = 0u64;

        c.observe(calm(20, 100), now);
        now += 500_000;

        for i in 0..100 {
            c.observe(
                LinkSample {
                    rtt: Duration::from_millis(500),
                    datagrams_sent: 1000 + i * 100,
                    datagrams_dropped: 100 + i * 50,
                },
                now,
            );
            now += 500_000;
        }

        assert!(c.current().bitrate >= RateConfig::new(MAX).min_bitrate);
    }

    /// И не поднимается выше заданного человеком потолка: решить, что
    /// канал вытянет больше, контроллер не вправе.
    #[test]
    fn bitrate_never_exceeds_the_ceiling() {
        let mut c = controller();
        let mut now = 0u64;

        for i in 0..200 {
            let decision = c.observe(calm(20, i * 100), now).expect("решение");
            assert!(decision.bitrate <= MAX);
            now += 500_000;
        }
    }

    /// Доля потерь считается **за интервал**, а не за всю сессию.
    ///
    /// Иначе после часа чистой работы минута обрыва дала бы доли
    /// процента, и контроллер не заметил бы обрыва вовсе.
    #[test]
    fn loss_is_measured_per_interval_not_cumulatively() {
        let mut c = controller();
        let mut now = 0u64;

        // Долгая чистая работа: миллион датаграмов без потерь.
        c.observe(calm(20, 1_000_000), now);
        now += 500_000;

        // Свежие потери: 100 из 1000 за интервал — 10 %.
        // Накопительно это 100 из 1 001 000, то есть 0.01 %.
        let decision = c
            .observe(
                LinkSample {
                    rtt: Duration::from_millis(20),
                    datagrams_sent: 1_001_000,
                    datagrams_dropped: 100,
                },
                now,
            )
            .expect("решение");

        assert_eq!(
            c.state(),
            LinkState::Lossy,
            "свежие потери утонули в накопленной статистике"
        );
        assert!(decision.fec_percent > 0);
    }

    /// Базовая линия — минимум окна, то есть оценка чистого полёта.
    #[test]
    fn baseline_tracks_the_minimum() {
        let mut c = controller();
        let mut now = 0u64;

        for rtt in [50u64, 30, 70, 20, 90] {
            c.observe(calm(rtt, 100), now);
            now += 500_000;
        }

        assert_eq!(c.baseline_rtt(), Some(Duration::from_millis(20)));
    }
}

#[cfg(test)]
mod loss_kind_tests {
    use super::*;

    const MAX: u32 = 15_000_000;

    /// **Потери, не зависящие от нагрузки, не должны сводить битрейт
    /// к дну.**
    ///
    /// Такие потери бывают от помех (Wi-Fi, мобильная сеть): их доля
    /// от нашего битрейта не зависит, и снижать его против них
    /// бессмысленно — картинка портится, а потери остаются.
    ///
    /// Дефект нашла проба `rate_probe`: на профиле «3 % потерь»
    /// первая версия за десять шагов свела 15 → 1 Мбит/с. Критерий
    /// этапа 6 («сессия пригодна для работы») был бы провален ровно
    /// тем механизмом, который писался ради него.
    #[test]
    fn steady_loss_does_not_collapse_bitrate() {
        let mut c = RateController::new(RateConfig::new(MAX));
        let mut now = 0u64;
        let mut sent = 0u64;
        let mut dropped = 0u64;

        // Чистый старт.
        for _ in 0..4 {
            sent += 800;
            c.observe(
                LinkSample {
                    rtt: Duration::from_millis(80),
                    datagrams_sent: sent,
                    datagrams_dropped: dropped,
                },
                now,
            );
            now += 500_000;
        }

        // Ровно 3 % потерь, не зависящих от битрейта.
        for _ in 0..20 {
            sent += 800;
            dropped += 24;
            c.observe(
                LinkSample {
                    rtt: Duration::from_millis(80),
                    datagrams_sent: sent,
                    datagrams_dropped: dropped,
                },
                now,
            );
            now += 500_000;
        }

        let decision = c.current();
        assert!(decision.fec_percent > 0, "паритет не включён при потерях");
        assert!(
            decision.bitrate >= 4_000_000,
            "битрейт сведён к {} — картинка непригодна, а потери от него не зависят",
            decision.bitrate
        );
    }

    /// Пара к предыдущему: если снижение ПОМОГАЕТ (потери падают
    /// вместе с битрейтом — признак перегрузки), контроллер обязан
    /// продолжать снижать.
    ///
    /// Без этого теста «никогда не снижать при потерях» прошло бы.
    #[test]
    fn loss_that_responds_to_bitrate_keeps_falling() {
        let mut c = RateController::new(RateConfig::new(MAX));
        let mut now = 0u64;
        let mut sent = 0u64;
        let mut dropped = 0u64;

        for _ in 0..4 {
            sent += 800;
            c.observe(
                LinkSample {
                    rtt: Duration::from_millis(80),
                    datagrams_sent: sent,
                    datagrams_dropped: dropped,
                },
                now,
            );
            now += 500_000;
        }

        // Потери убывают вдвое на каждом шаге — снижение работает.
        let mut loss_per_step = 200u64;
        for _ in 0..6 {
            sent += 800;
            dropped += loss_per_step;
            c.observe(
                LinkSample {
                    rtt: Duration::from_millis(80),
                    datagrams_sent: sent,
                    datagrams_dropped: dropped,
                },
                now,
            );
            now += 500_000;
            loss_per_step = (loss_per_step / 2).max(1);
        }

        assert!(
            c.current().bitrate < MAX / 2,
            "битрейт не снижался, хотя снижение уменьшало потери: {}",
            c.current().bitrate
        );
    }
}
