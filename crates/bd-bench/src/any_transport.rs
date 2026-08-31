//! Выбор транспорта на лету: заглушка или настоящий QUIC.
//!
//! # Почему enum, а не trait
//!
//! Транспортов ровно два, оба известны на этапе компиляции, и
//! добавлять третий не планируется: заглушка нужна для замеров без
//! сети, QUIC — для работы. Trait здесь дал бы динамическую
//! диспетчеризацию в горячем пути ради обобщённости, которой негде
//! пригодиться.
//!
//! Заглушку **не выбрасываем** после появления QUIC намеренно: она
//! единственный способ измерить стоимость пайплайна отдельно от
//! стоимости канала. Именно так была получена цифра «3.0 мс из 25»
//! на этапе 1, и без неё регресс пайплайна утонул бы в сетевом шуме.

#![cfg(all(windows, nvenc_available))]

use bd_core::metrics::FrameTimings;
use bd_core::time::Epoch;
use bd_transport::{
    LinkProfile, LoopbackTransport, PayloadKind, QuicTransport, ReassembledFrame, Result,
    TransportStats,
};
use std::net::SocketAddr;
use std::time::Duration;

/// Транспорт, выбранный при запуске.
pub enum AnyTransport {
    /// Канал в памяти с эмуляцией потерь и задержки.
    Loopback(Box<LoopbackTransport>),
    /// Настоящий QUIC поверх UDP.
    Quic(Box<QuicTransport>),
}

impl AnyTransport {
    /// Заглушка с заданным профилем канала.
    pub fn loopback(profile: LinkProfile, epoch: Epoch) -> Self {
        Self::Loopback(Box::new(LoopbackTransport::new(profile, epoch)))
    }

    /// Хост: ждать подключения клиента.
    pub fn host(bind: SocketAddr, timeout: Duration, epoch: Epoch) -> Result<Self> {
        Ok(Self::Quic(Box::new(QuicTransport::host(
            bind, timeout, epoch,
        )?)))
    }

    /// Клиент: подключиться к хосту.
    pub fn connect(server: SocketAddr, timeout: Duration, epoch: Epoch) -> Result<Self> {
        Ok(Self::Quic(Box::new(QuicTransport::connect(
            server, timeout, epoch,
        )?)))
    }

    /// Отправить кадр. Возвращает его номер.
    pub fn send(
        &mut self,
        kind: PayloadKind,
        keyframe: bool,
        data: &[u8],
        timings: &mut FrameTimings,
    ) -> Result<u64> {
        match self {
            Self::Loopback(t) => t.send(kind, keyframe, data, timings),
            Self::Quic(t) => t.send(kind, keyframe, data, timings),
        }
    }

    /// Забрать собранный кадр, если он есть.
    pub fn receive(&mut self, timings: &mut FrameTimings) -> Result<Option<ReassembledFrame>> {
        match self {
            Self::Loopback(t) => t.receive(timings),
            Self::Quic(t) => t.receive(timings),
        }
    }

    /// Счётчики канала.
    pub fn stats(&self) -> TransportStats {
        match self {
            Self::Loopback(t) => t.stats(),
            Self::Quic(t) => t.stats(),
        }
    }

    /// Круговое время.
    ///
    /// У заглушки это удвоенная задержка профиля — не измерение, а
    /// пересчёт заданного. Названо так же, потому что смысл для
    /// вызывающего один: сколько времени занимает путь туда-обратно.
    ///
    /// # Здесь был ноль, и это расходилось с документацией
    ///
    /// Комментарий выше обещал «удвоенную задержку профиля» с самого
    /// начала, а код возвращал `Duration::ZERO`. Пока RTT никто не
    /// показывал, расхождение ничего не стоило; на оверлее оно дало бы
    /// «RTT 0.0 мс» на профиле `mobile`, где задержка задана в 80 мс, —
    /// в том же отчёте, где строкой ниже печатается сам профиль.
    ///
    /// Это ровно та ошибка, что в находке 34: строки одного отчёта
    /// не обязаны сходиться между собой, и рано или поздно расходятся
    /// незаметно.
    pub fn rtt(&self) -> Duration {
        match self {
            Self::Loopback(t) => t.profile().delay * 2,
            Self::Quic(t) => t.rtt(),
        }
    }

    /// Наблюдён ли RTT на самом деле.
    ///
    /// У заглушки он вычислен из профиля, а не измерен. Разницу видно
    /// на оверлее: цифру, которая измерением не является, надо
    /// помечать, иначе она незаметно попадёт в выводы о сети.
    pub fn rtt_is_measured(&self) -> bool {
        matches!(self, Self::Quic(_))
    }

    /// Сколько датаграмов ждёт доставки.
    ///
    /// У QUIC этого числа нет: очередь внутри библиотеки. Ноль здесь
    /// означает «неизвестно», и вызывающий обязан это учитывать —
    /// иначе диагностика глубины очереди (находка 34) молча
    /// перестанет работать после перехода на сеть.
    pub fn in_flight(&self) -> usize {
        match self {
            Self::Loopback(t) => t.in_flight(),
            Self::Quic(_) => 0,
        }
    }

    /// Живо ли соединение.
    pub fn is_connected(&self) -> bool {
        match self {
            Self::Loopback(_) => true,
            Self::Quic(t) => t.is_connected(),
        }
    }

    /// Название для отчёта.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Loopback(_) => "заглушка",
            Self::Quic(_) => "QUIC",
        }
    }

    /// Настоящая ли это сеть.
    ///
    /// Нужно вердикту: критерий этапа 1 (≤ 25 мс) задан для
    /// localhost, и сравнивать с ним прогон через сеть нельзя —
    /// это было бы ровно той ошибкой, что в находке 29.
    pub fn is_real_network(&self) -> bool {
        matches!(self, Self::Quic(_))
    }
}

/// Что делает сторона с видеопотоком.
///
/// # Зачем это понадобилось
///
/// Заглушка замкнута сама на себя: одна сторона и шлёт, и принимает
/// свои же кадры. С QUIC сторон две, и если обе ведут себя как
/// заглушка, происходит вот что: обе шлют видео, обе нумеруют кадры
/// **с нуля**, и сборщик не может отличить свой кадр от чужого.
///
/// В прогоне это выглядело как «транспорт исказил кадр 0»: хост ждал
/// 40363 байта и получил 40219, а клиент — ровно наоборот. Байты не
/// портились; каждая сторона просто собрала кадр собеседника.
///
/// Настоящий продукт так не устроен (§3.1): хост захватывает и шлёт,
/// клиент принимает и показывает. Именно это здесь и задаётся.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoDirection {
    /// Обе стороны в одном процессе: шлём себе и принимаем от себя.
    Loopback,
    /// Хост: захватывает, кодирует, отправляет. Не принимает видео.
    SendOnly,
    /// Клиент: принимает, декодирует, показывает. Не отправляет.
    ReceiveOnly,
}

impl VideoDirection {
    /// Надо ли отправлять закодированные кадры.
    pub fn sends(self) -> bool {
        matches!(self, Self::Loopback | Self::SendOnly)
    }

    /// Надо ли принимать кадры и показывать их.
    pub fn receives(self) -> bool {
        matches!(self, Self::Loopback | Self::ReceiveOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_does_both() {
        assert!(VideoDirection::Loopback.sends());
        assert!(VideoDirection::Loopback.receives());
    }

    #[test]
    fn host_and_client_do_not_overlap() {
        // Дефект, который это чинит: обе стороны слали видео,
        // нумеровали с нуля, и каждая собирала кадр собеседника.
        assert!(VideoDirection::SendOnly.sends());
        assert!(!VideoDirection::SendOnly.receives());
        assert!(!VideoDirection::ReceiveOnly.sends());
        assert!(VideoDirection::ReceiveOnly.receives());
    }

    #[test]
    fn loopback_rtt_follows_the_profile() {
        // Заглушка обязана сообщать удвоенную задержку профиля, а не
        // ноль. Ноль читается как «сеть мгновенна», и на оверлее
        // расходился бы со строкой профиля в том же отчёте.
        //
        // Тест проверен на способность провалиться: с прежним
        // `Duration::ZERO` он падает на профиле MOBILE.
        let epoch = Epoch::new();
        let t = AnyTransport::loopback(LinkProfile::MOBILE, epoch);
        assert_eq!(
            t.rtt(),
            Duration::from_millis(160),
            "RTT заглушки не следует за профилем"
        );

        // И симметрично: идеальный канал обязан давать ноль, иначе
        // «следование профилю» превратилось бы в константу.
        let t = AnyTransport::loopback(LinkProfile::PERFECT, epoch);
        assert_eq!(t.rtt(), Duration::ZERO);
    }

    #[test]
    fn loopback_rtt_is_not_a_measurement() {
        // Цифра, полученная пересчётом заданного, не должна выдавать
        // себя за наблюдение: оверлей помечает её именно по этому
        // признаку.
        let epoch = Epoch::new();
        let t = AnyTransport::loopback(LinkProfile::LAN, epoch);
        assert!(!t.rtt_is_measured());
    }
}
