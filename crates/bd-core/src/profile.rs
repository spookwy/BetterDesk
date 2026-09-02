//! Профили сессии: «Качество», «Баланс», «Скорость».
//!
//! # Зачем профиль, если есть `--bitrate` и `--quality`
//!
//! `--quality` крутит **одну** величину — битрейт. Этого мало, и
//! видно это на простом вопросе: человек, выбравший «Скорость», хочет
//! не «поменьше мегабит», а **минимальную задержку** — и ради неё
//! готов на грубую картинку. А выбравший «Качество» готов ждать
//! лишние миллисекунды, лишь бы текст читался.
//!
//! Это разные размены, а не разные точки одной шкалы. Битрейт в них
//! меняется в разные стороны от разных причин:
//!
//! | | битрейт | частота | паритет | реакция |
//! |---|---|---|---|---|
//! | Качество | выше | 60 | щедрый | осторожная |
//! | Баланс | расчётный | 60 | умеренный | обычная |
//! | Скорость | ниже | 30 | скупой | резкая |
//!
//! # Почему «Скорость» снижает частоту, а не только битрейт
//!
//! При CBR битрейт делится на фактическое число кадров (находка 30):
//! те же 8 Мбит/с при 30 fps дают вдвое больше бит на кадр, чем при
//! 60. То есть половинная частота — это не «хуже вдвое», а размен
//! плавности на чёткость **при том же трафике**.
//!
//! Для работы с текстом (наш сценарий, §7.2) размен выгодный: читать
//! чёткий текст при 30 кадрах удобнее, чем мыльный при 60.
//!
//! # Почему профиль трогает и контроллер
//!
//! Контроллер битрейта — это тоже размен, и он обязан согласоваться
//! с выбором человека. «Скорость» означает «лучше просядь в
//! качестве, но не копи очередь»: реагировать надо резче и раньше.
//! «Качество» — наоборот: перетерпеть короткий всплеск задержки,
//! но не рушить картинку на каждом чихе.
//!
//! Оставь мы контроллер одинаковым, профиль отменялся бы через
//! несколько секунд после выбора — ровно как отменялся явный `--fec`
//! до появления `min_fec_percent`.

use crate::rate::RateConfig;

/// Профиль сессии — связка размена «задержка ↔ качество».
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionProfile {
    /// Картинка важнее отклика: чтение, просмотр, разбор документов.
    Quality,
    /// Умолчание: то, с чем проект работал до появления профилей.
    ///
    /// `#[default]` стоит здесь намеренно: без флага поведение обязано
    /// совпадать с тем, что было до профилей, иначе прежние замеры
    /// стали бы несравнимы с новыми.
    #[default]
    Balanced,
    /// Отклик важнее картинки: администрирование, работа руками.
    Speed,
}

impl SessionProfile {
    /// Разбор имени из аргумента командной строки или из оболочки.
    ///
    /// Принимает и русские имена: оболочка показывает их человеку, и
    /// требовать от неё перевода в английские значило бы завести
    /// вторую таблицу соответствий — то есть место, где копии
    /// разойдутся (находка 62).
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "quality" | "качество" => Some(Self::Quality),
            "balanced" | "balance" | "баланс" => Some(Self::Balanced),
            "speed" | "fast" | "скорость" => Some(Self::Speed),
            _ => None,
        }
    }

    /// Человекочитаемое имя.
    pub fn name(self) -> &'static str {
        match self {
            Self::Quality => "качество",
            Self::Balanced => "баланс",
            Self::Speed => "скорость",
        }
    }

    /// Множитель к расчётному битрейту.
    ///
    /// Числа те же, что были у `QualityPreset`, и это намеренно:
    /// они подобраны замером, и менять их заодно с переименованием
    /// значило бы смешать две правки в одну.
    pub fn bitrate_scale(self) -> (u32, u32) {
        match self {
            Self::Quality => (3, 2),
            Self::Balanced => (1, 1),
            Self::Speed => (1, 2),
        }
    }

    /// Применить профиль к расчётному битрейту.
    pub fn apply_bitrate(self, auto_bitrate: u32) -> u32 {
        let (num, den) = self.bitrate_scale();
        let scaled = auto_bitrate as u64 * num as u64 / den as u64;
        scaled.min(u32::MAX as u64) as u32
    }

    /// Предел частоты кодирования.
    ///
    /// `Speed` снижает до 30: при CBR это удваивает число бит на
    /// кадр, то есть меняет плавность на чёткость при том же трафике
    /// (находка 30). Для работы с текстом размен выгодный.
    pub fn fps_limit(self) -> u32 {
        match self {
            Self::Quality | Self::Balanced => 60,
            Self::Speed => 30,
        }
    }

    /// Избыточность FEC по умолчанию, проценты.
    ///
    /// Ноль у «Скорости» — не экономия ради экономии: паритет это
    /// **дополнительные датаграмы**, то есть больше очереди и больше
    /// задержки. Кто выбрал отклик, тот выбрал и это.
    ///
    /// У «Качества» щедро: там потеря кадра дороже лишнего трафика.
    pub fn default_fec_percent(self) -> u32 {
        match self {
            Self::Quality => 30,
            Self::Balanced => 20,
            Self::Speed => 0,
        }
    }

    /// Настроить контроллер битрейта под этот профиль.
    ///
    /// Контроллер обязан согласоваться с выбором человека, иначе
    /// профиль отменится сам собой через несколько секунд работы —
    /// ровно как отменялся явный `--fec` до `min_fec_percent`.
    pub fn tune(self, config: &mut RateConfig) {
        match self {
            // Осторожная реакция: короткий всплеск задержки перетерпеть
            // можно, а рушить картинку на каждом чихе — нельзя.
            Self::Quality => {
                config.congestion_ratio = 2.0;
                config.fall_step = 0.15;
                config.rise_step = 0.1;
            }
            Self::Balanced => {}
            // Резкая реакция: очередь копить нельзя, лучше сразу
            // просесть в качестве. Порог ниже — замечаем раньше.
            Self::Speed => {
                config.congestion_ratio = 1.25;
                config.fall_step = 0.35;
                config.rise_step = 0.15;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for profile in [
            SessionProfile::Quality,
            SessionProfile::Balanced,
            SessionProfile::Speed,
        ] {
            assert_eq!(
                SessionProfile::parse(profile.name()),
                Some(profile),
                "имя профиля не разбирается обратно"
            );
        }
    }

    #[test]
    fn english_and_russian_names_agree() {
        assert_eq!(
            SessionProfile::parse("speed"),
            SessionProfile::parse("скорость")
        );
        assert_eq!(
            SessionProfile::parse("quality"),
            SessionProfile::parse("качество")
        );
    }

    #[test]
    fn unknown_name_is_rejected() {
        // Молча брать умолчание нельзя: человек, опечатавшийся в
        // имени профиля, получил бы не то, что просил, без единого
        // признака ошибки.
        assert_eq!(SessionProfile::parse("быстро-быстро"), None);
        assert_eq!(SessionProfile::parse(""), None);
    }

    /// Профили обязаны идти в одном порядке по каждому размену.
    ///
    /// Проверяется не конкретное число, а **порядок**: подкрутить
    /// значение замером можно, а вот «Скорость», дающая больше бит,
    /// чем «Качество», — это уже не настройка, а дефект.
    #[test]
    fn profiles_are_ordered_consistently() {
        const AUTO: u32 = 10_000_000;

        let quality = SessionProfile::Quality;
        let balanced = SessionProfile::Balanced;
        let speed = SessionProfile::Speed;

        assert!(
            quality.apply_bitrate(AUTO) > balanced.apply_bitrate(AUTO),
            "«Качество» не даёт больше бит, чем «Баланс»"
        );
        assert!(
            balanced.apply_bitrate(AUTO) > speed.apply_bitrate(AUTO),
            "«Баланс» не даёт больше бит, чем «Скорость»"
        );

        assert!(
            quality.default_fec_percent() > speed.default_fec_percent(),
            "«Качество» защищено не лучше «Скорости»"
        );
        assert!(
            speed.fps_limit() <= balanced.fps_limit(),
            "«Скорость» кодирует чаще «Баланса»"
        );
    }

    /// «Скорость» реагирует на затор раньше и резче, чем «Качество».
    ///
    /// Это и есть смысл профиля: кто выбрал отклик, тот согласился
    /// платить качеством, но не задержкой.
    #[test]
    fn speed_reacts_sooner_than_quality() {
        let mut fast = RateConfig::new(10_000_000);
        let mut slow = RateConfig::new(10_000_000);
        SessionProfile::Speed.tune(&mut fast);
        SessionProfile::Quality.tune(&mut slow);

        assert!(
            fast.congestion_ratio < slow.congestion_ratio,
            "«Скорость» замечает затор не раньше «Качества»"
        );
        assert!(
            fast.fall_step > slow.fall_step,
            "«Скорость» снижает битрейт не резче «Качества»"
        );
    }

    /// «Баланс» не трогает контроллер вовсе.
    ///
    /// Умолчание обязано совпадать с тем, что было до появления
    /// профилей: иначе все прежние замеры оказались бы несравнимы с
    /// новыми, а причину пришлось бы искать в чужом коде.
    #[test]
    fn balanced_changes_nothing() {
        let base = RateConfig::new(10_000_000);
        let mut tuned = RateConfig::new(10_000_000);
        SessionProfile::Balanced.tune(&mut tuned);

        assert_eq!(tuned.congestion_ratio, base.congestion_ratio);
        assert_eq!(tuned.fall_step, base.fall_step);
        assert_eq!(tuned.rise_step, base.rise_step);
        assert_eq!(
            SessionProfile::Balanced.apply_bitrate(10_000_000),
            10_000_000,
            "«Баланс» изменил расчётный битрейт"
        );
    }
}
