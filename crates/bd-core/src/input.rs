//! События ввода: мышь и клавиатура.
//!
//! # Почему типы живут в `bd-core`
//!
//! Событие ввода рождается на клиенте, едет через транспорт и
//! применяется на хосте — то есть его видят три крейта, и ни один
//! из них не должен зависеть от двух других. Общий словарь для них
//! — `bd-core` (CLAUDE.md §4.2).
//!
//! Отсюда же требование портируемости (§4.2.1): здесь нет ни
//! `windows`, ни виртуальных кодов Win32. Клавиша описана
//! платформенно-независимым кодом, а перевод в `VK_*` живёт в
//! `bd-input`, где ему и место.
//!
//! # Почему координаты нормализованы
//!
//! Мышь передаётся не в пикселях, а в долях экрана (0.0–1.0).
//! Разрешения клиента и хоста не совпадают: окно клиента может быть
//! половинного размера, растянутым или на мониторе с другим DPI.
//! Пиксели пришлось бы пересчитывать на обеих сторонах, и любая
//! ошибка в пересчёте давала бы «уезжающий» курсор, который очень
//! трудно диагностировать.
//!
//! Нормализация переносит пересчёт в одно место — момент инжекта,
//! где известно настоящее разрешение целевого экрана.
//!
//! # Недоверенные данные
//!
//! Всё в этом модуле приходит **от постороннего по сети** и разбирается
//! до применения. Инжект ввода — первая по опасности поверхность атаки
//! (CLAUDE.md §8.5), поэтому здесь нет ни одного `unwrap`, а разбор
//! проверяет каждое поле (см. [`InputEvent::parse`]).

use crate::time::Timestamp;

/// Кнопка мыши.
///
/// Только те кнопки, что есть у любой мыши. Боковые (X1/X2) добавятся,
/// когда понадобятся: неиспользуемый вариант в протоколе — это код,
/// который никто не проверял.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Левая кнопка.
    Left,
    /// Правая кнопка.
    Right,
    /// Средняя (колесо нажато).
    Middle,
}

impl MouseButton {
    /// Код кнопки в wire-формате.
    const fn code(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 3,
        }
    }

    /// Разобрать код кнопки.
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(MouseButton::Left),
            2 => Some(MouseButton::Right),
            3 => Some(MouseButton::Middle),
            _ => None,
        }
    }
}

/// Положение курсора в долях экрана.
///
/// Обе координаты лежат в `[0.0, 1.0]`: `(0.0, 0.0)` — левый верхний
/// угол, `(1.0, 1.0)` — правый нижний. Конструктор зажимает значения
/// в этот диапазон, поэтому вне его позиция существовать не может.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MousePosition {
    x: f32,
    y: f32,
}

impl MousePosition {
    /// Создать позицию, зажав координаты в `[0.0, 1.0]`.
    ///
    /// Зажим, а не отказ: курсор, выехавший за край окна на пиксель,
    /// — обычное дело, и терять из-за этого событие незачем. `NaN`
    /// становится нулём: он пришёл бы из испорченного пакета, а
    /// сравнения с `NaN` ложны и зажим его бы не поймал.
    pub fn new(x: f32, y: f32) -> Self {
        Self {
            x: clamp_unit(x),
            y: clamp_unit(y),
        }
    }

    /// Доля по горизонтали, `0.0`–`1.0`.
    pub fn x(self) -> f32 {
        self.x
    }

    /// Доля по вертикали, `0.0`–`1.0`.
    pub fn y(self) -> f32 {
        self.y
    }

    /// Перевести в пиксели экрана заданного размера.
    ///
    /// Единственное место, где нормализованная позиция становится
    /// пикселями. Результат не выходит за границы экрана, потому что
    /// доли уже зажаты, — но крайнее значение `1.0` дало бы координату
    /// ровно на границе, а последний допустимый пиксель на единицу
    /// меньше, поэтому здесь ещё одно ограничение.
    pub fn to_pixels(self, width: u32, height: u32) -> (i32, i32) {
        let px = (self.x * width as f32) as i32;
        let py = (self.y * height as f32) as i32;
        (
            px.clamp(0, width.saturating_sub(1) as i32),
            py.clamp(0, height.saturating_sub(1) as i32),
        )
    }

    /// Построить позицию из пикселей в окне заданного размера.
    ///
    /// Обратная операция к [`Self::to_pixels`]. Нулевой размер окна
    /// даёт `(0, 0)`: делить на ноль нельзя, а окно нулевого размера
    /// — штатное состояние свёрнутого окна.
    pub fn from_pixels(x: i32, y: i32, width: u32, height: u32) -> Self {
        if width == 0 || height == 0 {
            return Self::new(0.0, 0.0);
        }
        Self::new(x as f32 / width as f32, y as f32 / height as f32)
    }
}

/// Зажать значение в `[0.0, 1.0]`, превратив `NaN` в `0.0`.
fn clamp_unit(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// Событие ввода, идущее от клиента к хосту.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Курсор переместился.
    MouseMove {
        /// Новое положение в долях экрана.
        position: MousePosition,
    },
    /// Кнопка мыши нажата или отпущена.
    MouseButton {
        /// Какая кнопка.
        button: MouseButton,
        /// `true` — нажата, `false` — отпущена.
        pressed: bool,
        /// Где произошло. Передаётся вместе с кликом, а не отдельным
        /// событием: иначе при потере предыдущего `MouseMove` клик
        /// пришёлся бы не туда, куда целился человек.
        position: MousePosition,
    },
    /// Колесо мыши прокручено.
    MouseScroll {
        /// Вертикальная прокрутка в «щелчках». Положительное — от себя.
        delta_y: f32,
        /// Горизонтальная прокрутка. Положительное — вправо.
        delta_x: f32,
    },
    /// Клавиша нажата или отпущена.
    Key {
        /// Платформенно-независимый код клавиши.
        key: KeyCode,
        /// `true` — нажата, `false` — отпущена.
        pressed: bool,
    },
    /// Клиент потерял фокус — отпустить всё, что зажато.
    ///
    /// Без этого события клавиши «залипают»: человек нажал Alt,
    /// переключился в другое окно, отпустил Alt уже там — а хост
    /// об отпускании не узнал и остался с зажатым модификатором.
    /// Критерий этапа 2 требует, чтобы этого не происходило
    /// (docs/roadmap.md).
    ReleaseAll,
}

/// Платформенно-независимый код клавиши.
///
/// Это **физическая** клавиша, а не символ: раскладка применяется на
/// хосте. Иначе при разных раскладках у сторон человек печатал бы одно,
/// а получал другое.
///
/// Значения совпадают с кодами USB HID Usage Page 0x07 — общепринятым
/// словарём, который понимают и Windows, и Linux, и macOS. Свой
/// произвольный набор здесь был бы ошибкой: его пришлось бы
/// сопоставлять с системным на каждой платформе.
///
/// `Default` — код 0, что в HID означает «клавиша не задана».
/// Нужен для массивов фиксированного размера в отслеживании
/// зажатых клавиш.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct KeyCode(pub u16);

impl KeyCode {
    /// Клавиша `A` (HID 0x04). Начало буквенного диапазона.
    pub const A: Self = Self(0x04);
    /// Клавиша `Z` (HID 0x1D). Конец буквенного диапазона.
    pub const Z: Self = Self(0x1D);
    /// Клавиша `1` (HID 0x1E).
    pub const DIGIT_1: Self = Self(0x1E);
    /// Enter (HID 0x28).
    pub const ENTER: Self = Self(0x28);
    /// Escape (HID 0x29).
    pub const ESCAPE: Self = Self(0x29);
    /// Backspace (HID 0x2A).
    pub const BACKSPACE: Self = Self(0x2A);
    /// Tab (HID 0x2B).
    pub const TAB: Self = Self(0x2B);
    /// Пробел (HID 0x2C).
    pub const SPACE: Self = Self(0x2C);
    /// Левый Control (HID 0xE0). Начало диапазона модификаторов.
    pub const LEFT_CTRL: Self = Self(0xE0);
    /// Левый Shift (HID 0xE1).
    pub const LEFT_SHIFT: Self = Self(0xE1);
    /// Левый Alt (HID 0xE2).
    pub const LEFT_ALT: Self = Self(0xE2);
    /// Левая Windows (HID 0xE3).
    pub const LEFT_META: Self = Self(0xE3);
    /// Правый Control (HID 0xE4).
    pub const RIGHT_CTRL: Self = Self(0xE4);
    /// Правый Shift (HID 0xE5).
    pub const RIGHT_SHIFT: Self = Self(0xE5);
    /// Правый Alt (HID 0xE6).
    pub const RIGHT_ALT: Self = Self(0xE6);
    /// Правая Windows (HID 0xE7). Конец диапазона модификаторов.
    pub const RIGHT_META: Self = Self(0xE7);

    /// Модификатор ли это (Ctrl, Shift, Alt, Win).
    ///
    /// Модификаторы требуют отдельного внимания: именно они «залипают»
    /// при потере фокуса, и именно их состояние надо восстанавливать
    /// после переподключения.
    pub const fn is_modifier(self) -> bool {
        self.0 >= Self::LEFT_CTRL.0 && self.0 <= Self::RIGHT_META.0
    }
}

/// Размер события в wire-формате: 1 байт вида + 8 байт полезной части.
const EVENT_SIZE: usize = 9;

/// Коды видов события в wire-формате.
mod kind {
    pub const MOUSE_MOVE: u8 = 1;
    pub const MOUSE_BUTTON: u8 = 2;
    pub const MOUSE_SCROLL: u8 = 3;
    pub const KEY: u8 = 4;
    pub const RELEASE_ALL: u8 = 5;
}

impl InputEvent {
    /// Записать событие в wire-формате.
    ///
    /// Формат фиксированной длины ([`EVENT_SIZE`] байт): вид события
    /// плюс поля, дополненные нулями. Переменная длина сэкономила бы
    /// байты, но потребовала бы разбора длины — то есть ещё одного
    /// поля, которому нельзя доверять. При 9 байтах на событие и сотне
    /// событий в секунду экономить нечего: это 0.9 КБ/с против
    /// 15 Мбит/с видео.
    ///
    /// Координаты кодируются как `u16` от 0 до 65535 вместо `f32`:
    /// точность 1/65536 экрана — это доли пикселя даже на 8K, а
    /// целочисленный формат исключает `NaN` и денормализованные
    /// значения на входе разбора.
    pub fn encode(&self, out: &mut [u8; EVENT_SIZE]) {
        out.fill(0);
        match *self {
            InputEvent::MouseMove { position } => {
                out[0] = kind::MOUSE_MOVE;
                write_position(&mut out[1..5], position);
            }
            InputEvent::MouseButton {
                button,
                pressed,
                position,
            } => {
                out[0] = kind::MOUSE_BUTTON;
                write_position(&mut out[1..5], position);
                out[5] = button.code();
                out[6] = u8::from(pressed);
            }
            InputEvent::MouseScroll { delta_y, delta_x } => {
                out[0] = kind::MOUSE_SCROLL;
                // Прокрутка передаётся сотыми долями щелчка: тачпады
                // дают дробные значения, а целые щелчки потеряли бы
                // плавность. Диапазон i16 — ±327 щелчков за событие,
                // много больше любого реального жеста.
                let sy = (delta_y * 100.0).clamp(-32768.0, 32767.0) as i16;
                let sx = (delta_x * 100.0).clamp(-32768.0, 32767.0) as i16;
                out[1..3].copy_from_slice(&sy.to_le_bytes());
                out[3..5].copy_from_slice(&sx.to_le_bytes());
            }
            InputEvent::Key { key, pressed } => {
                out[0] = kind::KEY;
                out[1..3].copy_from_slice(&key.0.to_le_bytes());
                out[3] = u8::from(pressed);
            }
            InputEvent::ReleaseAll => {
                out[0] = kind::RELEASE_ALL;
            }
        }
    }

    /// Разобрать событие из wire-формата.
    ///
    /// **Это парсер недоверенных данных** (CLAUDE.md §8.5): байты
    /// пришли от постороннего. Поэтому проверяется всё — длина, код
    /// вида, код кнопки — и ни одна ветка не паникует. Неизвестный код
    /// даёт `None`, а не «событие по умолчанию»: применить не то, что
    /// прислали, хуже, чем не применить ничего.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        // Ровно EVENT_SIZE: короткий буфер — обрезанный пакет, длинный
        // — не наш формат. Принимать «хотя бы столько» значило бы
        // молча съедать мусор в хвосте.
        if bytes.len() != EVENT_SIZE {
            return None;
        }

        match bytes[0] {
            kind::MOUSE_MOVE => Some(InputEvent::MouseMove {
                position: read_position(&bytes[1..5])?,
            }),
            kind::MOUSE_BUTTON => Some(InputEvent::MouseButton {
                position: read_position(&bytes[1..5])?,
                button: MouseButton::from_code(bytes[5])?,
                // Любое ненулевое значение — «нажата». Требовать ровно
                // 1 значило бы отбрасывать событие из-за мелочи,
                // которая ни на что не влияет.
                pressed: bytes[6] != 0,
            }),
            kind::MOUSE_SCROLL => {
                let sy = i16::from_le_bytes([bytes[1], bytes[2]]);
                let sx = i16::from_le_bytes([bytes[3], bytes[4]]);
                Some(InputEvent::MouseScroll {
                    delta_y: f32::from(sy) / 100.0,
                    delta_x: f32::from(sx) / 100.0,
                })
            }
            kind::KEY => Some(InputEvent::Key {
                key: KeyCode(u16::from_le_bytes([bytes[1], bytes[2]])),
                pressed: bytes[3] != 0,
            }),
            kind::RELEASE_ALL => Some(InputEvent::ReleaseAll),
            _ => None,
        }
    }

    /// Размер события в wire-формате.
    pub const fn wire_size() -> usize {
        EVENT_SIZE
    }

    /// Равны ли события с точностью wire-формата.
    ///
    /// # Зачем это нужно вместо `==`
    ///
    /// Координаты и прокрутка кодируются целыми числами, поэтому
    /// `encode` → `parse` возвращает **не то же самое значение**:
    /// `0.25` становится `0.24998856`. Разница — 1/65536 экрана, доли
    /// пикселя даже на 8K, то есть формат работает как задумано.
    ///
    /// Но сравнение через `==` этого не знает и объявляет такие
    /// события искажёнными. Именно так и вышло при первом прогоне
    /// `input_probe`: проверка целостности сообщила о «77 искажённых
    /// событиях» там, где транспорт был исправен.
    ///
    /// Поэтому проверять целостность передачи надо этим методом:
    /// он сравнивает с тем допуском, который формат обещает.
    /// Для точного сравнения (например, что событие не подменилось
    /// другим) `==` по-прежнему верен.
    pub fn eq_within_wire_precision(&self, other: &Self) -> bool {
        /// Допуск: два шага квантования `u16`. Один шаг — предел
        /// ошибки округления, два — запас на кодирование и разбор.
        const POSITION_EPS: f32 = 2.0 / 65535.0;
        /// Прокрутка кодируется сотыми долями щелчка.
        const SCROLL_EPS: f32 = 0.02;

        match (*self, *other) {
            (InputEvent::MouseMove { position: a }, InputEvent::MouseMove { position: b }) => {
                close(a.x(), b.x(), POSITION_EPS) && close(a.y(), b.y(), POSITION_EPS)
            }

            (
                InputEvent::MouseButton {
                    button: ba,
                    pressed: pa,
                    position: a,
                },
                InputEvent::MouseButton {
                    button: bb,
                    pressed: pb,
                    position: b,
                },
            ) => {
                ba == bb
                    && pa == pb
                    && close(a.x(), b.x(), POSITION_EPS)
                    && close(a.y(), b.y(), POSITION_EPS)
            }

            (
                InputEvent::MouseScroll {
                    delta_y: ya,
                    delta_x: xa,
                },
                InputEvent::MouseScroll {
                    delta_y: yb,
                    delta_x: xb,
                },
            ) => close(ya, yb, SCROLL_EPS) && close(xa, xb, SCROLL_EPS),

            // Клавиши и ReleaseAll кодируются точно: там сравнивать
            // с допуском нечего, и `==` — верный ответ.
            (a, b) => a == b,
        }
    }
}

/// Близки ли значения в пределах допуска.
fn close(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() <= eps
}

/// Записать позицию как две доли `u16`.
fn write_position(out: &mut [u8], position: MousePosition) {
    let x = (position.x() * f32::from(u16::MAX)) as u16;
    let y = (position.y() * f32::from(u16::MAX)) as u16;
    out[0..2].copy_from_slice(&x.to_le_bytes());
    out[2..4].copy_from_slice(&y.to_le_bytes());
}

/// Прочитать позицию из двух долей `u16`.
///
/// Отказать здесь невозможно: любое значение `u16` — корректная доля.
/// `Option` в сигнатуре для единообразия с остальным разбором.
fn read_position(bytes: &[u8]) -> Option<MousePosition> {
    let x = u16::from_le_bytes([bytes[0], bytes[1]]);
    let y = u16::from_le_bytes([bytes[2], bytes[3]]);
    Some(MousePosition::new(
        f32::from(x) / f32::from(u16::MAX),
        f32::from(y) / f32::from(u16::MAX),
    ))
}

/// Событие ввода с номером и отметкой времени.
///
/// # Зачем номер
///
/// События едут unreliable datagrams (CLAUDE.md §5.3): ретрансмит
/// устаревшего нажатия хуже его потери. Но потеря отпускания клавиши
/// оставляет её зажатой навсегда, поэтому получатель обязан замечать
/// пропуски — для этого и номер.
///
/// Порядок тоже не гарантирован: событие, пришедшее позже отправленного
/// после него, надо отбросить, иначе «нажал-отпустил» превратится в
/// «отпустил-нажал» и клавиша залипнет.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SequencedInput {
    /// Номер события, растёт монотонно.
    pub sequence: u64,
    /// Когда событие произошло на клиенте.
    pub timestamp: Timestamp,
    /// Само событие.
    pub event: InputEvent,
}

/// Размер `SequencedInput` в wire-формате.
const SEQUENCED_SIZE: usize = 8 + 8 + EVENT_SIZE;

impl SequencedInput {
    /// Записать в wire-формате.
    pub fn encode(&self) -> [u8; SEQUENCED_SIZE] {
        let mut out = [0u8; SEQUENCED_SIZE];
        out[0..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..16].copy_from_slice(&self.timestamp.as_micros().to_le_bytes());
        let mut event = [0u8; EVENT_SIZE];
        self.event.encode(&mut event);
        out[16..].copy_from_slice(&event);
        out
    }

    /// Разобрать из wire-формата. Недоверенные данные (§8.5).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != SEQUENCED_SIZE {
            return None;
        }
        let sequence = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let micros = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        Some(Self {
            sequence,
            timestamp: Timestamp::from_micros(micros),
            event: InputEvent::parse(&bytes[16..])?,
        })
    }

    /// Размер в wire-формате.
    pub const fn wire_size() -> usize {
        SEQUENCED_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: InputEvent) -> InputEvent {
        let mut buf = [0u8; EVENT_SIZE];
        event.encode(&mut buf);
        InputEvent::parse(&buf).expect("разбор своего же формата")
    }

    #[test]
    fn mouse_move_survives_roundtrip() {
        let event = InputEvent::MouseMove {
            position: MousePosition::new(0.25, 0.75),
        };
        let back = roundtrip(event);
        let InputEvent::MouseMove { position } = back else {
            panic!("вид события изменился: {back:?}");
        };
        // Кодирование в u16 теряет точность, но не больше 1/65536.
        assert!((position.x() - 0.25).abs() < 1e-4, "x = {}", position.x());
        assert!((position.y() - 0.75).abs() < 1e-4, "y = {}", position.y());
    }

    #[test]
    fn every_button_and_state_survives_roundtrip() {
        // Точного равенства позиций здесь не требуется: доли кодируются
        // в u16, и 0.5 возвращается как 0.49999237. Это заложенная
        // потеря в 1/65536 экрана — доли пикселя даже на 8K. Проверять
        // надо то, что формат обещает: кнопку, состояние и позицию
        // с точностью формата.
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            for pressed in [true, false] {
                let event = InputEvent::MouseButton {
                    button,
                    pressed,
                    position: MousePosition::new(0.5, 0.5),
                };
                let back = roundtrip(event);
                let InputEvent::MouseButton {
                    button: b,
                    pressed: p,
                    position,
                } = back
                else {
                    panic!("вид события изменился: {back:?}");
                };
                assert_eq!(b, button, "кнопка искажена");
                assert_eq!(p, pressed, "состояние кнопки искажено");
                assert!(
                    (position.x() - 0.5).abs() < 1e-4 && (position.y() - 0.5).abs() < 1e-4,
                    "позиция уехала дальше точности формата: {position:?}"
                );
            }
        }
    }

    #[test]
    fn key_survives_roundtrip() {
        let event = InputEvent::Key {
            key: KeyCode::LEFT_SHIFT,
            pressed: true,
        };
        assert_eq!(roundtrip(event), event);
    }

    #[test]
    fn scroll_keeps_sign_and_magnitude() {
        let event = InputEvent::MouseScroll {
            delta_y: -3.5,
            delta_x: 1.25,
        };
        let back = roundtrip(event);
        let InputEvent::MouseScroll { delta_y, delta_x } = back else {
            panic!("вид события изменился");
        };
        assert!((delta_y + 3.5).abs() < 0.01, "delta_y = {delta_y}");
        assert!((delta_x - 1.25).abs() < 0.01, "delta_x = {delta_x}");
    }

    #[test]
    fn position_is_clamped_to_unit_range() {
        // Курсор, выехавший за окно, — обычное дело. Зажимаем, а не
        // теряем событие.
        let p = MousePosition::new(-0.5, 2.0);
        assert_eq!(p.x(), 0.0);
        assert_eq!(p.y(), 1.0);
    }

    #[test]
    fn nan_position_becomes_zero() {
        // NaN пришёл бы из испорченного пакета. Обычный clamp его не
        // ловит: все сравнения с NaN ложны, и он прошёл бы насквозь,
        // а дальше превратился бы в мусорную координату при инжекте.
        let p = MousePosition::new(f32::NAN, f32::NAN);
        assert_eq!(p.x(), 0.0);
        assert_eq!(p.y(), 0.0);
    }

    #[test]
    fn pixels_never_exceed_screen_bounds() {
        // Доля 1.0 при ширине 1920 дала бы координату 1920 — на пиксель
        // за краем. Последний допустимый — 1919.
        let p = MousePosition::new(1.0, 1.0);
        assert_eq!(p.to_pixels(1920, 1080), (1919, 1079));
    }

    #[test]
    fn pixel_roundtrip_is_stable() {
        let p = MousePosition::from_pixels(960, 540, 1920, 1080);
        assert_eq!(p.to_pixels(1920, 1080), (960, 540));
    }

    #[test]
    fn zero_sized_window_does_not_divide_by_zero() {
        // Свёрнутое окно имеет нулевой размер — это не сбой.
        let p = MousePosition::from_pixels(10, 10, 0, 0);
        assert_eq!(p.x(), 0.0);
        assert_eq!(p.y(), 0.0);
    }

    #[test]
    fn wrong_length_is_rejected() {
        // Обрезанный или подклеенный пакет не должен разбираться.
        assert!(InputEvent::parse(&[]).is_none());
        assert!(InputEvent::parse(&[kind::MOUSE_MOVE; EVENT_SIZE - 1]).is_none());
        assert!(InputEvent::parse(&[kind::MOUSE_MOVE; EVENT_SIZE + 1]).is_none());
    }

    #[test]
    fn unknown_kind_is_rejected() {
        // Применить не то, что прислали, хуже, чем не применить ничего.
        let mut buf = [0u8; EVENT_SIZE];
        buf[0] = 200;
        assert!(InputEvent::parse(&buf).is_none());
    }

    #[test]
    fn unknown_button_is_rejected() {
        let mut buf = [0u8; EVENT_SIZE];
        buf[0] = kind::MOUSE_BUTTON;
        buf[5] = 99;
        assert!(InputEvent::parse(&buf).is_none());
    }

    #[test]
    fn parser_never_panics_on_arbitrary_bytes() {
        // Дешёвая замена фаззингу до появления cargo-fuzz (§10.3).
        // Парсер обязан отвечать None, а не паниковать, на любом входе.
        for kind_byte in 0..=255u8 {
            for filler in [0u8, 1, 0x7F, 0x80, 0xFF] {
                let mut buf = [filler; EVENT_SIZE];
                buf[0] = kind_byte;
                let _ = InputEvent::parse(&buf);
                for len in 0..EVENT_SIZE * 2 {
                    let bytes = vec![filler; len];
                    let _ = InputEvent::parse(&bytes);
                }
            }
        }
    }

    #[test]
    fn sequenced_survives_roundtrip() {
        let input = SequencedInput {
            sequence: 42,
            timestamp: Timestamp::from_micros(123_456),
            event: InputEvent::Key {
                key: KeyCode::ESCAPE,
                pressed: false,
            },
        };
        let bytes = input.encode();
        assert_eq!(SequencedInput::parse(&bytes), Some(input));
    }

    #[test]
    fn sequenced_rejects_wrong_length() {
        assert!(SequencedInput::parse(&[0u8; 4]).is_none());
        assert!(SequencedInput::parse(&[0u8; SEQUENCED_SIZE + 1]).is_none());
    }

    #[test]
    fn modifiers_are_recognized() {
        // От этого признака зависит обработка залипания (ReleaseAll).
        assert!(KeyCode::LEFT_CTRL.is_modifier());
        assert!(KeyCode::RIGHT_META.is_modifier());
        assert!(!KeyCode::A.is_modifier());
        assert!(!KeyCode::SPACE.is_modifier());
    }

    #[test]
    fn wire_precision_comparison_accepts_roundtrip() {
        // Ровно тот случай, на котором споткнулась первая версия
        // `input_probe`: 0.25 возвращается как 0.24998856, и `==`
        // объявляет событие искажённым, хотя формат отработал верно.
        let original = InputEvent::MouseMove {
            position: MousePosition::new(0.25, 0.75),
        };
        let back = roundtrip(original);
        assert_ne!(back, original, "иначе тест ничего не проверяет");
        assert!(
            back.eq_within_wire_precision(&original),
            "потеря точности формата не должна считаться искажением: {back:?}"
        );
    }

    #[test]
    fn wire_precision_comparison_still_catches_real_differences() {
        // Обратная сторона: допуск не должен превращать сравнение
        // в «всегда равно». Иначе проверка целостности перестанет
        // ловить настоящую порчу данных.
        let a = InputEvent::MouseMove {
            position: MousePosition::new(0.25, 0.75),
        };
        let far = InputEvent::MouseMove {
            position: MousePosition::new(0.30, 0.75),
        };
        assert!(
            !a.eq_within_wire_precision(&far),
            "0.05 — это не округление"
        );

        // Другой вид события — никогда не равен.
        let key = InputEvent::Key {
            key: KeyCode::A,
            pressed: true,
        };
        assert!(!a.eq_within_wire_precision(&key));

        // Кнопка и состояние сравниваются точно даже при близких
        // координатах: клик левой вместо правой — это порча.
        let left = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
            position: MousePosition::new(0.5, 0.5),
        };
        let right = InputEvent::MouseButton {
            button: MouseButton::Right,
            pressed: true,
            position: MousePosition::new(0.5, 0.5),
        };
        assert!(!left.eq_within_wire_precision(&right));

        let released = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: false,
            position: MousePosition::new(0.5, 0.5),
        };
        assert!(!left.eq_within_wire_precision(&released));
    }

    #[test]
    fn wire_size_matches_encoded_length() {
        // Размер объявлен в публичном API; расхождение с фактическим
        // сломало бы фрагментацию молча.
        assert_eq!(InputEvent::wire_size(), EVENT_SIZE);
        let input = SequencedInput {
            sequence: 1,
            timestamp: Timestamp::from_micros(0),
            event: InputEvent::ReleaseAll,
        };
        assert_eq!(SequencedInput::wire_size(), input.encode().len());
    }
}
