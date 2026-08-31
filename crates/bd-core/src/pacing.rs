//! Ограничение частоты кодирования.
//!
//! # Зачем это нужно
//!
//! DXGI Desktop Duplication отдаёт кадр при каждом изменении экрана,
//! а не 60 раз в секунду. При активной работе это легко даёт 150–250
//! кадров в секунду.
//!
//! Кодировать их все — ошибка, и не только из-за нагрузки. Битрейт
//! при CBR фиксирован: 15 Мбит/с делятся на **фактическое** число
//! кадров. При 222 кадрах в секунду на кадр приходится втрое меньше
//! бит, чем при 60, — и энкодер огрубляет картинку, чтобы уложиться.
//!
//! Измерено (docs/measurements.md): при 222 кадрах/с медиана
//! разностного кадра — 2301 Б, при 32 кадрах/с — 12 071 Б. Разница
//! в 5.2 раза, и она видна глазом как периодическая пикселизация.
//!
//! Лишние кадры к тому же бессмысленны: монитор клиента показывает
//! 60–144 кадра в секунду, и всё сверх этого просто отбрасывается
//! после того, как на него потрачены биты канала.
//!
//! # Почему не просто «спать»
//!
//! Ожидание в горячем пути остановило бы захват: пока поток спит,
//! DXGI копит изменения, и следующий кадр придёт устаревшим. Поэтому
//! лимитер не ждёт, а **отвечает на вопрос**, пора ли кодировать
//! очередной кадр. Пропущенный кадр просто не кодируется — его
//! содержимое всё равно войдёт в следующий.

use crate::time::{now, Instant};
use std::time::Duration;

/// Ограничитель частоты кодирования.
///
/// Не блокирует поток: [`should_encode`](Self::should_encode) отвечает
/// «да» или «нет» и не ждёт.
#[derive(Debug, Clone)]
pub struct FrameLimiter {
    /// Минимальный интервал между закодированными кадрами.
    min_interval: Duration,
    /// Момент последнего разрешённого кадра.
    last: Option<Instant>,
    /// Сколько кадров пропущено.
    skipped: u64,
    /// Сколько кадров разрешено.
    passed: u64,
}

impl FrameLimiter {
    /// Ограничитель на заданную частоту кадров.
    ///
    /// `fps` равный нулю отключает ограничение — это осмысленный
    /// режим для замеров, где нужна вся частота захвата.
    pub fn new(fps: u32) -> Self {
        Self {
            min_interval: if fps == 0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(1.0 / fps as f64)
            },
            last: None,
            skipped: 0,
            passed: 0,
        }
    }

    /// Ограничитель без ограничения.
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    /// Включено ли ограничение.
    pub fn is_limited(&self) -> bool {
        !self.min_interval.is_zero()
    }

    /// Пора ли кодировать кадр, пришедший в момент `at`.
    ///
    /// Ответ «да» отмечает момент как последний закодированный,
    /// поэтому вызывать метод нужно ровно один раз на кадр.
    pub fn should_encode(&mut self, at: Instant) -> bool {
        if self.min_interval.is_zero() {
            self.passed += 1;
            return true;
        }

        match self.last {
            Some(last) if at.saturating_duration_since(last) < self.min_interval => {
                self.skipped += 1;
                false
            }
            Some(last) => {
                // Отметка сдвигается на **целое число интервалов**, а
                // не ставится по времени прихода.
                //
                // # Почему это важно
                //
                // Источник неравномерен: DXGI отдаёт кадр на изменение
                // экрана, интервалы гуляют от 3 до 9 мс.
                //
                // При отметке «по времени прихода» кадр, пришедший на
                // долю миллисекунды раньше слота, отвергается — а
                // следующий приходит только через 9 мс. Слот пропущен
                // целиком, и так раз за разом: сетка каждый раз
                // отсчитывается от последнего **принятого** кадра,
                // то есть всегда чуть позже, чем надо.
                //
                // Измерено: при лимите 60 неравномерный поток давал
                // **48** кадров/с. Частота уезжает ВНИЗ, а не вверх.
                //
                // (Первая гипотеза была обратной — «дрейф вверх до 76
                // кадров/с». Тест опроверг её за один прогон, ещё до
                // того как была написана хоть строка правки. Тот же
                // приём, что спас день работы в находке 41.)
                //
                // Сдвиг по сетке слотов удерживает среднюю частоту
                // равной заданной независимо от того, как рвано
                // приходит источник.
                //
                // # Почему нужен догоняющий цикл
                //
                // После паузы (UAC, статичный экран) `last` отстаёт на
                // много интервалов сразу. Сдвиг на один интервал
                // заставил бы лимитер пропускать всё подряд, пока
                // сетка не догонит настоящее время. Считаем сразу,
                // сколько интервалов прошло.
                let behind = at.saturating_duration_since(last);
                let whole = (behind.as_nanos() / self.min_interval.as_nanos()).max(1);
                // Кап на случай очень долгой паузы: сетка не должна
                // уезжать в далёкое прошлое, иначе следующая пауза
                // снова считалась бы «догоняем».
                let advance = self
                    .min_interval
                    .saturating_mul(whole.min(u32::MAX as u128) as u32);
                self.last = Some(last + advance);
                self.passed += 1;
                true
            }
            None => {
                self.last = Some(at);
                self.passed += 1;
                true
            }
        }
    }

    /// То же для текущего момента.
    pub fn should_encode_now(&mut self) -> bool {
        self.should_encode(now())
    }

    /// Сколько кадров пропущено ограничителем.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Сколько кадров пропущено дальше в пайплайн.
    pub fn passed(&self) -> u64 {
        self.passed
    }

    /// Сбросить состояние.
    ///
    /// Нужно после паузы в потоке кадров: иначе первый кадр после
    /// возобновления будет отвергнут из-за давно устаревшей отметки.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Момент через `ms` миллисекунд от `base`.
    fn after(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn limiter_passes_frames_at_the_target_rate() {
        let mut limiter = FrameLimiter::new(60); // интервал 16.67 мс
        let t0 = now();

        assert!(limiter.should_encode(t0), "первый кадр всегда проходит");
        // Через 10 мс — рано.
        assert!(!limiter.should_encode(after(t0, 10)));
        // Через 20 мс от начала — пора.
        assert!(limiter.should_encode(after(t0, 20)));
    }

    #[test]
    fn limiter_drops_the_excess_of_a_fast_source() {
        // Ровно тот случай, ради которого лимитер и написан: источник
        // отдаёт 240 кадров/с, а кодировать надо 60.
        let mut limiter = FrameLimiter::new(60);
        let t0 = now();

        let mut passed = 0;
        // 240 кадров за секунду — по кадру каждые 4.16 мс.
        for i in 0..240 {
            let at = t0 + Duration::from_micros(i * 4_167);
            if limiter.should_encode(at) {
                passed += 1;
            }
        }

        // Ожидаем около 60, с допуском на округление интервалов.
        assert!(
            (58..=62).contains(&passed),
            "из 240 кадров должно пройти ~60, прошло {passed}"
        );
        assert_eq!(limiter.passed(), passed as u64);
        assert_eq!(limiter.skipped(), 240 - passed as u64);
    }

    #[test]
    fn irregular_source_does_not_overshoot_the_limit() {
        // Прогон показал 76.4 кадра/с при лимите 60 — на кадр
        // приходилось 48 КБ вместо 61 КБ, и это было видно глазом
        // как пикселизация.
        //
        // Прежний тест этого не ловил, потому что подавал **идеально
        // равномерный** источник в 240 кадров/с. DXGI так себя не
        // ведёт: он отдаёт кадр на изменение экрана, интервалы рваные.
        //
        // Механизм ошибки: отметка ставилась по времени **прихода**
        // кадра, а не по расписанию. Кадр, пришедший на 0.5 мс позже
        // слота, сдвигал следующий слот на те же 0.5 мс, и ошибка
        // копилась — частота уезжала вверх на десятки процентов.
        //
        // Здесь источник неравномерный: интервалы гуляют от 3 до 9 мс.
        let mut limiter = FrameLimiter::new(60);
        let t0 = now();

        let mut passed = 0;
        let mut at = t0;
        // Ровно секунда рваного потока.
        let mut step = 0u64;
        while at.saturating_duration_since(t0) < Duration::from_secs(1) {
            if limiter.should_encode(at) {
                passed += 1;
            }
            // Псевдослучайный шаг 3–9 мс: тот самый рваный поток.
            step = (step * 1103515245 + 12345) % 7;
            at += Duration::from_millis(3 + step);
        }

        // За секунду при лимите 60 должно пройти около 60 кадров.
        // Тест проверен на способность провалиться: с отметкой по
        // времени прихода проходит заметно больше.
        assert!(
            (57..=63).contains(&passed),
            "лимит 60 кадров/с, за секунду прошло {passed}"
        );
    }

    #[test]
    fn slow_source_passes_everything() {
        // Источник медленнее лимита ограничиваться не должен:
        // иначе мы теряли бы кадры на ровном месте.
        let mut limiter = FrameLimiter::new(60);
        let t0 = now();

        for i in 0..30 {
            // 30 кадров в секунду — вдвое медленнее лимита.
            let at = t0 + Duration::from_millis(i * 33);
            assert!(limiter.should_encode(at), "кадр {i} обязан пройти");
        }
        assert_eq!(limiter.skipped(), 0);
    }

    #[test]
    fn unlimited_passes_everything() {
        let mut limiter = FrameLimiter::unlimited();
        assert!(!limiter.is_limited());
        let t0 = now();
        for i in 0..100 {
            assert!(limiter.should_encode(t0 + Duration::from_micros(i)));
        }
        assert_eq!(limiter.skipped(), 0);
    }

    #[test]
    fn reset_allows_the_next_frame_immediately() {
        let mut limiter = FrameLimiter::new(60);
        let t0 = now();
        assert!(limiter.should_encode(t0));
        assert!(!limiter.should_encode(after(t0, 1)));
        limiter.reset();
        assert!(
            limiter.should_encode(after(t0, 2)),
            "после сброса кадр должен пройти сразу"
        );
    }

    #[test]
    fn zero_fps_means_unlimited_not_a_panic() {
        // Ноль — осмысленный режим, а не ошибка: деления на ноль
        // здесь быть не должно.
        let mut limiter = FrameLimiter::new(0);
        assert!(!limiter.is_limited());
        assert!(limiter.should_encode(now()));
    }
}
