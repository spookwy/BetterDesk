//! Отслеживание зажатых клавиш и кнопок.
//!
//! # Зачем это нужно
//!
//! Критерий этапа 2: «ни одна клавиша не залипает при alt-tab и потере
//! фокуса» (docs/roadmap.md). Залипание — не редкость, а **поведение
//! по умолчанию**, если ничего не делать, и происходит оно двумя
//! путями:
//!
//! 1. **Потеря фокуса.** Человек зажал Alt, нажал Tab, ушёл в другое
//!    окно и отпустил Alt уже там. Клиент отпускания не увидел — его
//!    получило другое окно, — а хост остался с зажатым Alt навсегда.
//!
//! 2. **Потеря пакета.** События ввода едут unreliable datagrams
//!    (CLAUDE.md §5.3), и «отпустил» может не дойти. Нажатие потерять
//!    не страшно — человек нажмёт ещё раз. Потеря отпускания
//!    **необратима**: клавиша останется зажатой, и никакие последующие
//!    события её не освободят.
//!
//! Асимметрия здесь принципиальна: нажатие и отпускание стоят
//! по-разному, поэтому и обращаться с ними надо по-разному.
//!
//! # Почему трекер живёт на обеих сторонах
//!
//! На клиенте он знает, что отпустить при потере фокуса. На хосте —
//! что отпустить при разрыве сессии, иначе чужие клавиши останутся
//! зажатыми у живого человека за тем компьютером.

use bd_core::input::{InputEvent, KeyCode, MouseButton};

/// Сколько клавиш может быть зажато одновременно.
///
/// Восьми хватает: аппаратные клавиатуры без N-key rollover обычно
/// регистрируют не более 6 обычных клавиш плюс модификаторы, а
/// осмысленных аккордов длиннее четырёх не бывает. Ограничение здесь
/// не оптимизация, а защита: без него сторона, шлющая только нажатия,
/// заставила бы нас расти без предела.
const MAX_TRACKED_KEYS: usize = 8;

/// Что произошло при обработке события.
///
/// Возвращается вместо `()`, потому что вызывающему нужно знать не
/// только «принято», но и «это дубликат» — повторные нажатия от
/// автоповтора клавиатуры не должны идти в сеть.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackOutcome {
    /// Состояние изменилось, событие имеет смысл передать.
    Changed,
    /// Состояние уже было таким. Автоповтор или дубликат из сети.
    Redundant,
    /// Нажатие отброшено: зажато предельное число клавиш.
    Overflow,
}

/// Состояние зажатых клавиш и кнопок мыши.
///
/// Хранит **только то, что зажато сейчас**, а не историю. Размер
/// ограничен [`MAX_TRACKED_KEYS`], потому что состояние наполняется
/// данными из сети.
#[derive(Debug, Default, Clone)]
pub struct InputTracker {
    /// Зажатые клавиши. Массив, а не множество: их единицы, линейный
    /// поиск по восьми элементам быстрее хеширования, а аллокаций нет
    /// вовсе — это горячий путь, событие на каждое нажатие.
    keys: heapless_vec::KeyVec,
    /// Зажатые кнопки мыши. Битовая маска: кнопок всего три.
    buttons: u8,
}

/// Минимальный вектор фиксированной ёмкости.
///
/// Свой, а не `heapless`: зависимость ради восьми `u16` не окупается,
/// а `Vec` дал бы аллокацию в горячем пути.
mod heapless_vec {
    use super::MAX_TRACKED_KEYS;
    use bd_core::input::KeyCode;

    /// Вектор клавиш фиксированной ёмкости.
    #[derive(Debug, Default, Clone)]
    pub struct KeyVec {
        items: [KeyCode; MAX_TRACKED_KEYS],
        len: usize,
    }

    impl KeyVec {
        /// Есть ли клавиша в наборе.
        pub fn contains(&self, key: KeyCode) -> bool {
            self.items[..self.len].contains(&key)
        }

        /// Добавить клавишу. `false` — набор полон.
        pub fn push(&mut self, key: KeyCode) -> bool {
            if self.len >= MAX_TRACKED_KEYS {
                return false;
            }
            self.items[self.len] = key;
            self.len += 1;
            true
        }

        /// Убрать клавишу. `false` — её там не было.
        pub fn remove(&mut self, key: KeyCode) -> bool {
            let Some(index) = self.items[..self.len].iter().position(|&k| k == key) else {
                return false;
            };
            // Порядок не важен, поэтому дыра затыкается последним
            // элементом, а не сдвигом хвоста.
            self.len -= 1;
            self.items[index] = self.items[self.len];
            true
        }

        /// Зажатые клавиши.
        pub fn as_slice(&self) -> &[KeyCode] {
            &self.items[..self.len]
        }

        /// Сколько зажато.
        pub fn len(&self) -> usize {
            self.len
        }

        /// Пуст ли набор.
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        /// Очистить.
        pub fn clear(&mut self) {
            self.len = 0;
        }
    }
}

impl InputTracker {
    /// Новый трекер: ничего не зажато.
    pub fn new() -> Self {
        Self::default()
    }

    /// Учесть событие.
    ///
    /// События, не меняющие состояния зажатости (движение, прокрутка),
    /// всегда дают [`TrackOutcome::Changed`]: они не дубликаты, даже
    /// если координаты совпали.
    pub fn track(&mut self, event: &InputEvent) -> TrackOutcome {
        match *event {
            InputEvent::Key { key, pressed } => {
                if pressed {
                    if self.keys.contains(key) {
                        // Автоповтор клавиатуры шлёт нажатия десятками
                        // в секунду, пока клавишу держат. Пропускать их
                        // в сеть — тратить канал на то, что хост и так
                        // сделает сам своим автоповтором.
                        TrackOutcome::Redundant
                    } else if self.keys.push(key) {
                        TrackOutcome::Changed
                    } else {
                        TrackOutcome::Overflow
                    }
                } else if self.keys.remove(key) {
                    TrackOutcome::Changed
                } else {
                    // Отпускание незажатой клавиши. Бывает штатно:
                    // клавишу нажали до подключения либо нажатие
                    // потерялось. Передать всё равно стоит — хост мог
                    // считать её зажатой.
                    TrackOutcome::Changed
                }
            }
            InputEvent::MouseButton {
                button, pressed, ..
            } => {
                let mask = button_mask(button);
                let was = self.buttons & mask != 0;
                if pressed == was {
                    return TrackOutcome::Redundant;
                }
                if pressed {
                    self.buttons |= mask;
                } else {
                    self.buttons &= !mask;
                }
                TrackOutcome::Changed
            }
            InputEvent::ReleaseAll => {
                let had_something = !self.keys.is_empty() || self.buttons != 0;
                self.keys.clear();
                self.buttons = 0;
                if had_something {
                    TrackOutcome::Changed
                } else {
                    TrackOutcome::Redundant
                }
            }
            InputEvent::MouseMove { .. } | InputEvent::MouseScroll { .. } => TrackOutcome::Changed,
        }
    }

    /// События, отпускающие всё зажатое.
    ///
    /// Возвращается **список конкретных отпусканий**, а не одно
    /// [`InputEvent::ReleaseAll`]. Разница существенна: `ReleaseAll` —
    /// это команда, которую надо доставить, а она может потеряться
    /// (unreliable datagrams). Конкретные отпускания идемпотентны:
    /// повторное отпускание незажатой клавиши безвредно, поэтому их
    /// можно слать многократно, не боясь навредить.
    ///
    /// Порядок: сначала обычные клавиши, потом модификаторы. Если
    /// отпустить Ctrl раньше, чем C в аккорде Ctrl+C, хост на миг
    /// увидит одиночное нажатие C — то есть напечатанную букву вместо
    /// копирования.
    pub fn release_events(&self) -> impl Iterator<Item = InputEvent> + '_ {
        let plain = self
            .keys
            .as_slice()
            .iter()
            .filter(|k| !k.is_modifier())
            .map(|&key| InputEvent::Key {
                key,
                pressed: false,
            });

        let modifiers = self
            .keys
            .as_slice()
            .iter()
            .filter(|k| k.is_modifier())
            .map(|&key| InputEvent::Key {
                key,
                pressed: false,
            });

        let buttons = [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
            .into_iter()
            .filter(move |&b| self.buttons & button_mask(b) != 0)
            .map(|button| InputEvent::MouseButton {
                button,
                pressed: false,
                // Позиция при отпускании не важна: кнопку надо отпустить
                // там, где курсор сейчас. Ноль здесь означал бы прыжок
                // в угол экрана, поэтому вызывающий обязан подставить
                // текущую позицию, если она ему известна.
                position: bd_core::input::MousePosition::new(0.0, 0.0),
            });

        plain.chain(modifiers).chain(buttons)
    }

    /// Зажата ли клавиша.
    pub fn is_key_pressed(&self, key: KeyCode) -> bool {
        self.keys.contains(key)
    }

    /// Зажата ли кнопка мыши.
    pub fn is_button_pressed(&self, button: MouseButton) -> bool {
        self.buttons & button_mask(button) != 0
    }

    /// Сколько клавиш зажато.
    pub fn pressed_key_count(&self) -> usize {
        self.keys.len()
    }

    /// Зажато ли хоть что-нибудь.
    pub fn is_idle(&self) -> bool {
        self.keys.is_empty() && self.buttons == 0
    }

    /// Забыть всё зажатое, не порождая событий.
    ///
    /// Для случая, когда отпускать уже нечего: сессия разорвана, и
    /// слать на ту сторону некуда.
    pub fn reset(&mut self) {
        self.keys.clear();
        self.buttons = 0;
    }
}

/// Битовая маска кнопки мыши.
const fn button_mask(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::input::MousePosition;

    fn key(key: KeyCode, pressed: bool) -> InputEvent {
        InputEvent::Key { key, pressed }
    }

    fn button(button: MouseButton, pressed: bool) -> InputEvent {
        InputEvent::MouseButton {
            button,
            pressed,
            position: MousePosition::new(0.5, 0.5),
        }
    }

    #[test]
    fn press_and_release_are_tracked() {
        let mut t = InputTracker::new();
        assert!(t.is_idle());

        assert_eq!(t.track(&key(KeyCode::A, true)), TrackOutcome::Changed);
        assert!(t.is_key_pressed(KeyCode::A));
        assert!(!t.is_idle());

        assert_eq!(t.track(&key(KeyCode::A, false)), TrackOutcome::Changed);
        assert!(!t.is_key_pressed(KeyCode::A));
        assert!(t.is_idle());
    }

    #[test]
    fn autorepeat_is_reported_as_redundant() {
        // Клавиатура шлёт нажатия десятками в секунду, пока клавишу
        // держат. Гнать их в сеть незачем: хост повторит сам.
        let mut t = InputTracker::new();
        assert_eq!(t.track(&key(KeyCode::A, true)), TrackOutcome::Changed);
        for _ in 0..10 {
            assert_eq!(t.track(&key(KeyCode::A, true)), TrackOutcome::Redundant);
        }
        assert_eq!(t.pressed_key_count(), 1, "автоповтор раздул состояние");
    }

    #[test]
    fn release_of_unpressed_key_still_propagates() {
        // Хост мог считать клавишу зажатой, если нажатие потерялось
        // по дороге. Глушить такое отпускание — значит оставить
        // залипание неисправимым.
        let mut t = InputTracker::new();
        assert_eq!(t.track(&key(KeyCode::A, false)), TrackOutcome::Changed);
    }

    #[test]
    fn release_events_free_everything_that_was_held() {
        let mut t = InputTracker::new();
        t.track(&key(KeyCode::LEFT_ALT, true));
        t.track(&key(KeyCode::TAB, true));
        t.track(&button(MouseButton::Left, true));

        let events: Vec<_> = t.release_events().collect();
        assert_eq!(events.len(), 3, "отпущено не всё: {events:?}");

        // Применение этих событий обязано опустошить трекер — иначе
        // они не решают задачу, ради которой существуют.
        let mut check = t.clone();
        for e in &events {
            check.track(e);
        }
        assert!(check.is_idle(), "после отпускания осталось зажатое");
    }

    #[test]
    fn modifiers_are_released_after_plain_keys() {
        // Ctrl+C: если отпустить Ctrl первым, хост на миг увидит
        // одиночное C — то есть напечатает букву вместо копирования.
        let mut t = InputTracker::new();
        t.track(&key(KeyCode::LEFT_CTRL, true));
        t.track(&key(KeyCode::A, true));

        let events: Vec<_> = t.release_events().collect();
        let position_of = |target: KeyCode| {
            events
                .iter()
                .position(|e| matches!(e, InputEvent::Key { key, .. } if *key == target))
                .expect("клавиша не отпущена")
        };
        assert!(
            position_of(KeyCode::A) < position_of(KeyCode::LEFT_CTRL),
            "модификатор отпущен раньше обычной клавиши: {events:?}"
        );
    }

    #[test]
    fn release_all_clears_state() {
        let mut t = InputTracker::new();
        t.track(&key(KeyCode::LEFT_SHIFT, true));
        t.track(&button(MouseButton::Right, true));
        assert!(!t.is_idle());

        assert_eq!(t.track(&InputEvent::ReleaseAll), TrackOutcome::Changed);
        assert!(t.is_idle());
        // Повторный ReleaseAll ничего не меняет — сообщаем об этом,
        // чтобы вызывающий не гнал его в сеть без нужды.
        assert_eq!(t.track(&InputEvent::ReleaseAll), TrackOutcome::Redundant);
    }

    #[test]
    fn key_overflow_is_rejected_not_grown() {
        // Состояние наполняется данными из сети. Сторона, шлющая одни
        // нажатия, не должна заставлять нас расти без предела.
        let mut t = InputTracker::new();
        for i in 0..MAX_TRACKED_KEYS {
            let outcome = t.track(&key(KeyCode(0x04 + i as u16), true));
            assert_eq!(outcome, TrackOutcome::Changed, "клавиша {i} не принята");
        }
        assert_eq!(
            t.track(&key(KeyCode(0xFF), true)),
            TrackOutcome::Overflow,
            "переполнение не обнаружено"
        );
        assert_eq!(t.pressed_key_count(), MAX_TRACKED_KEYS);
    }

    #[test]
    fn buttons_are_tracked_independently() {
        let mut t = InputTracker::new();
        t.track(&button(MouseButton::Left, true));
        assert!(t.is_button_pressed(MouseButton::Left));
        assert!(!t.is_button_pressed(MouseButton::Right));

        // Повторное нажатие той же кнопки — дубликат.
        assert_eq!(
            t.track(&button(MouseButton::Left, true)),
            TrackOutcome::Redundant
        );

        t.track(&button(MouseButton::Left, false));
        assert!(t.is_idle());
    }

    #[test]
    fn move_and_scroll_never_change_pressed_state() {
        let mut t = InputTracker::new();
        t.track(&key(KeyCode::A, true));
        let before = t.pressed_key_count();

        t.track(&InputEvent::MouseMove {
            position: MousePosition::new(0.1, 0.2),
        });
        t.track(&InputEvent::MouseScroll {
            delta_y: 1.0,
            delta_x: 0.0,
        });

        assert_eq!(t.pressed_key_count(), before);
        assert!(t.is_key_pressed(KeyCode::A));
    }

    #[test]
    fn reset_forgets_without_events() {
        let mut t = InputTracker::new();
        t.track(&key(KeyCode::A, true));
        t.reset();
        assert!(t.is_idle());
        assert_eq!(t.release_events().count(), 0);
    }

    #[test]
    fn removing_key_keeps_others_intact() {
        // Дыра затыкается последним элементом — легко потерять соседа.
        let mut t = InputTracker::new();
        for i in 0..4u16 {
            t.track(&key(KeyCode(0x04 + i), true));
        }
        t.track(&key(KeyCode(0x05), false));

        assert_eq!(t.pressed_key_count(), 3);
        assert!(t.is_key_pressed(KeyCode(0x04)));
        assert!(!t.is_key_pressed(KeyCode(0x05)));
        assert!(t.is_key_pressed(KeyCode(0x06)));
        assert!(t.is_key_pressed(KeyCode(0x07)));
    }
}
