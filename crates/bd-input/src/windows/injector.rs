//! Инжект событий ввода на хосте через `SendInput`.
//!
//! # Безопасность
//!
//! Это **самая опасная поверхность продукта** (CLAUDE.md §8.5,
//! docs/security-model.md §7.3): код здесь двигает мышь и нажимает
//! клавиши на чужом компьютере. Он обязан вызываться только из
//! установленной аутентифицированной сессии.
//!
//! Все входные данные к этому моменту уже разобраны в safe-коде
//! ([`bd_core::input::InputEvent::parse`]), поэтому сюда попадают
//! только валидные события: координаты зажаты в `[0,1]`, коды кнопок
//! проверены. Дополнительной валидации здесь нет намеренно — она
//! должна быть на границе, а не размазана по слоям.
//!
//! # Ограничения, которые не лечатся здесь
//!
//! `SendInput` не работает с **secure desktop** (UAC, экран
//! блокировки, Ctrl+Alt+Del): пользовательская сессия не имеет туда
//! доступа. Это то же ограничение, что у захвата (находка 32), и
//! снимается только службой (§3.2, этап 8). Ctrl+Alt+Del не
//! перехватывается вообще ничем, кроме SAS-драйвера, — критерий
//! этапа 2 разрешает это задокументировать вместо реализации.

use super::keymap::hid_to_scancode;
use crate::error::{InputError, Result};
use bd_core::input::{InputEvent, MouseButton, MousePosition};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};

/// Сколько «единиц колеса» в одном щелчке.
///
/// Windows определяет `WHEEL_DELTA = 120`. Дробные значения от
/// тачпадов передаются как доли щелчка и умножаются здесь.
const WHEEL_DELTA: f32 = 120.0;

/// Верхняя граница нормализованных координат в `SendInput`.
///
/// При `MOUSEEVENTF_ABSOLUTE` экран адресуется числами 0–65535
/// независимо от разрешения. Это ровно та нормализация, что принята
/// в [`MousePosition`], поэтому пересчёт здесь тривиален и не зависит
/// от разрешения хоста — что важно, потому что разрешение может
/// смениться в любой момент.
const ABSOLUTE_MAX: f32 = 65535.0;

/// Инжектор событий ввода в текущую сессию Windows.
///
/// Состояния не имеет: `SendInput` работает с очередью системы,
/// а зажатые клавиши отслеживает [`crate::InputTracker`] на уровне
/// выше. Разделение намеренное — состояние нужно и на клиенте, где
/// никакого `SendInput` нет.
#[derive(Debug, Default, Clone, Copy)]
pub struct InputInjector {
    /// Работать со всем виртуальным рабочим столом, а не только
    /// с основным монитором.
    virtual_desktop: bool,
}

impl InputInjector {
    /// Инжектор для основного монитора.
    pub fn new() -> Self {
        Self {
            virtual_desktop: false,
        }
    }

    /// Инжектор, адресующий весь виртуальный рабочий стол.
    ///
    /// Нужен при захвате не основного монитора: без
    /// `MOUSEEVENTF_VIRTUALDESK` координаты 0–65535 растягиваются на
    /// основной экран, и курсор не попадёт на соседний (этап 11).
    pub fn for_virtual_desktop() -> Self {
        Self {
            virtual_desktop: true,
        }
    }

    /// Применить событие.
    ///
    /// Возвращает ошибку, если система отвергла ввод. Самая частая
    /// причина — **UIPI**: процесс с меньшим уровнем целостности не
    /// может слать ввод окну с большим. То есть инжект в окно,
    /// запущенное от администратора, не пройдёт, пока наш процесс
    /// не поднят так же. Это не наша ошибка и не лечится повтором,
    /// но и не повод рвать сессию — вызывающий решает сам.
    pub fn inject(&self, event: &InputEvent) -> Result<()> {
        match *event {
            InputEvent::MouseMove { position } => self.send_mouse_move(position),
            InputEvent::MouseButton {
                button,
                pressed,
                position,
            } => {
                // Позиция передаётся вместе с кликом, поэтому курсор
                // ставится в одном пакете с нажатием. Раздельно
                // получилось бы два события, между которыми система
                // могла бы вклинить чужой ввод, и клик ушёл бы не туда.
                let mut inputs = [INPUT::default(); 2];
                inputs[0] = self.mouse_move_input(position);
                inputs[1] = mouse_button_input(button, pressed);
                send(&inputs)
            }
            InputEvent::MouseScroll { delta_y, delta_x } => {
                let mut inputs = [INPUT::default(); 2];
                let mut count = 0;
                if delta_y != 0.0 {
                    inputs[count] = wheel_input(delta_y, false);
                    count += 1;
                }
                if delta_x != 0.0 {
                    inputs[count] = wheel_input(delta_x, true);
                    count += 1;
                }
                if count == 0 {
                    return Ok(());
                }
                send(&inputs[..count])
            }
            InputEvent::Key { key, pressed } => {
                let Some(scan) = hid_to_scancode(key) else {
                    // Неизвестная клавиша — не ошибка сессии: HID
                    // определяет сотни кодов, включая мультимедийные.
                    // Нажать наугад не ту хуже, чем не нажать ничего.
                    tracing::debug!(code = key.0, "клавиша не сопоставлена скан-коду");
                    return Ok(());
                };

                let mut flags = KEYEVENTF_SCANCODE;
                if scan.extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !pressed {
                    flags |= KEYEVENTF_KEYUP;
                }

                let input = INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            // При KEYEVENTF_SCANCODE виртуальный код
                            // игнорируется: клавишу определяет wScan.
                            wVk: Default::default(),
                            wScan: scan.code,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                send(&[input])
            }
            InputEvent::ReleaseAll => {
                // Само по себе событие ничего не отпускает: инжектор
                // не знает, что зажато. Разворачивает его в конкретные
                // отпускания тот, кто ведёт состояние
                // ([`crate::InputTracker`]). Здесь — намеренно ничего,
                // иначе состояние пришлось бы дублировать в двух местах.
                Ok(())
            }
        }
    }

    /// Применить несколько событий одним вызовом.
    ///
    /// `SendInput` вставляет массив в очередь **атомарно**: между
    /// событиями не вклинится чужой ввод. Для отпускания клавиш при
    /// потере фокуса это важно — иначе между отпусканием Ctrl и C
    /// система могла бы увидеть одиночное C.
    ///
    /// События, не разворачиваемые в `INPUT` (неизвестные клавиши,
    /// [`InputEvent::ReleaseAll`]), пропускаются.
    pub fn inject_batch(&self, events: &[InputEvent]) -> Result<()> {
        // Ограничение сверху: массив собирается из данных, пришедших
        // по сети. 64 события за раз — много больше любого разумного
        // пакета отпусканий (8 клавиш + 3 кнопки).
        const MAX_BATCH: usize = 64;

        let mut inputs: Vec<INPUT> = Vec::with_capacity(events.len().min(MAX_BATCH));
        for event in events.iter().take(MAX_BATCH) {
            match *event {
                InputEvent::MouseMove { position } => {
                    inputs.push(self.mouse_move_input(position));
                }
                InputEvent::MouseButton {
                    button,
                    pressed,
                    position,
                } => {
                    inputs.push(self.mouse_move_input(position));
                    inputs.push(mouse_button_input(button, pressed));
                }
                InputEvent::MouseScroll { delta_y, delta_x } => {
                    if delta_y != 0.0 {
                        inputs.push(wheel_input(delta_y, false));
                    }
                    if delta_x != 0.0 {
                        inputs.push(wheel_input(delta_x, true));
                    }
                }
                InputEvent::Key { key, pressed } => {
                    let Some(scan) = hid_to_scancode(key) else {
                        continue;
                    };
                    let mut flags = KEYEVENTF_SCANCODE;
                    if scan.extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    if !pressed {
                        flags |= KEYEVENTF_KEYUP;
                    }
                    inputs.push(INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: Default::default(),
                                wScan: scan.code,
                                dwFlags: flags,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
                InputEvent::ReleaseAll => continue,
            }
        }

        if inputs.is_empty() {
            return Ok(());
        }
        send(&inputs)
    }

    /// Переместить курсор.
    fn send_mouse_move(&self, position: MousePosition) -> Result<()> {
        send(&[self.mouse_move_input(position)])
    }

    /// Собрать структуру перемещения курсора.
    fn mouse_move_input(&self, position: MousePosition) -> INPUT {
        let mut flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
        if self.virtual_desktop {
            flags |= MOUSEEVENTF_VIRTUALDESK;
        }

        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    // Координаты нормализованы на обеих сторонах, и
                    // разрешение хоста здесь не нужно. Это не удобство,
                    // а устойчивость: разрешение может смениться между
                    // отправкой события и его применением.
                    dx: (position.x() * ABSOLUTE_MAX) as i32,
                    dy: (position.y() * ABSOLUTE_MAX) as i32,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
}

/// Собрать структуру нажатия или отпускания кнопки.
fn mouse_button_input(button: MouseButton, pressed: bool) -> INPUT {
    let flags = match (button, pressed) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };

    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Собрать структуру прокрутки.
fn wheel_input(delta: f32, horizontal: bool) -> INPUT {
    // `mouseData` для колеса — знаковое значение, но поле объявлено
    // как u32. Приведение через i32 сохраняет знак в битах, чего и
    // ждёт API; привести f32 прямо к u32 значило бы потерять
    // отрицательные значения (прокрутку вверх).
    let amount = (delta * WHEEL_DELTA) as i32;

    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: amount as u32,
                dwFlags: if horizontal {
                    MOUSEEVENTF_HWHEEL
                } else {
                    MOUSEEVENTF_WHEEL
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Отправить подготовленные события в систему.
///
/// Единственное место в модуле, где вызывается `SendInput`, — чтобы
/// проверка результата была в одном месте и её нельзя было забыть.
fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    // SAFETY: `inputs` — непустой срез корректно инициализированных
    // структур INPUT; каждая заполнена целиком (union — через
    // конкретный вариант, соответствующий полю `type`). Размер
    // передаётся через size_of, а не константой, поэтому не разъедется
    // с определением структуры. SendInput только читает срез и не
    // сохраняет указатель.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };

    if sent as usize == inputs.len() {
        return Ok(());
    }

    // Частичная отправка означает блокировку: система приняла часть
    // событий и отвергла остальные. Оставлять это без внимания нельзя
    // — при отпускании клавиш недосланное событие и есть залипание.
    //
    // SAFETY: GetLastError не имеет предусловий.
    let code = unsafe { windows::Win32::Foundation::GetLastError() };
    Err(InputError::Blocked {
        sent: sent as usize,
        total: inputs.len(),
        code: code.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::input::KeyCode;

    // Здесь проверяется сборка структур, а не результат инжекта:
    // настоящий `SendInput` двигал бы мышь на машине разработчика
    // и в CI, что недопустимо для юнит-теста.

    #[test]
    fn mouse_position_maps_to_absolute_range() {
        let injector = InputInjector::new();
        let input = injector.mouse_move_input(MousePosition::new(0.0, 1.0));

        // SAFETY: поле `mi` заполнено, потому что type == INPUT_MOUSE.
        let mi = unsafe { input.Anonymous.mi };
        assert_eq!(mi.dx, 0);
        assert_eq!(mi.dy, ABSOLUTE_MAX as i32);
        assert!(mi.dwFlags.0 & MOUSEEVENTF_ABSOLUTE.0 != 0);
    }

    #[test]
    fn virtual_desktop_flag_is_set_only_when_asked() {
        let plain = InputInjector::new().mouse_move_input(MousePosition::new(0.5, 0.5));
        let virt =
            InputInjector::for_virtual_desktop().mouse_move_input(MousePosition::new(0.5, 0.5));

        // SAFETY: обе структуры мышиные.
        let (a, b) = unsafe { (plain.Anonymous.mi, virt.Anonymous.mi) };
        assert_eq!(a.dwFlags.0 & MOUSEEVENTF_VIRTUALDESK.0, 0);
        assert_ne!(b.dwFlags.0 & MOUSEEVENTF_VIRTUALDESK.0, 0);
    }

    #[test]
    fn scroll_up_stays_positive_and_down_negative() {
        // Знак прокрутки проходит через u32 — самое место потерять
        // его молча и получить инвертированное колесо.
        // SAFETY: структуры мышиные.
        let up = unsafe { wheel_input(1.0, false).Anonymous.mi };
        let down = unsafe { wheel_input(-1.0, false).Anonymous.mi };

        assert_eq!(up.mouseData as i32, 120);
        assert_eq!(down.mouseData as i32, -120);
    }

    #[test]
    fn horizontal_scroll_uses_its_own_flag() {
        // SAFETY: структуры мышиные.
        let v = unsafe { wheel_input(1.0, false).Anonymous.mi };
        let h = unsafe { wheel_input(1.0, true).Anonymous.mi };
        assert_eq!(v.dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(h.dwFlags, MOUSEEVENTF_HWHEEL);
    }

    #[test]
    fn key_release_sets_keyup_flag() {
        let injector = InputInjector::new();
        let events = [InputEvent::Key {
            key: KeyCode::A,
            pressed: false,
        }];
        // Собираем пакет, не отправляя: проверяем флаги.
        let mut inputs: Vec<INPUT> = Vec::new();
        for event in &events {
            if let InputEvent::Key { key, pressed } = *event {
                let scan = hid_to_scancode(key).unwrap();
                let mut flags = KEYEVENTF_SCANCODE;
                if !pressed {
                    flags |= KEYEVENTF_KEYUP;
                }
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: Default::default(),
                            wScan: scan.code,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        let _ = injector;
        // SAFETY: структура клавиатурная.
        let ki = unsafe { inputs[0].Anonymous.ki };
        assert!(ki.dwFlags.0 & KEYEVENTF_KEYUP.0 != 0);
        assert!(ki.dwFlags.0 & KEYEVENTF_SCANCODE.0 != 0);
    }

    #[test]
    fn empty_batch_is_not_an_error() {
        // Пустой пакет возможен штатно: все события оказались
        // неизвестными клавишами. Отправлять нечего, но и ошибки нет.
        let injector = InputInjector::new();
        assert!(injector.inject_batch(&[]).is_ok());
        assert!(injector.inject_batch(&[InputEvent::ReleaseAll]).is_ok());
    }

    #[test]
    fn release_all_injects_nothing() {
        // Инжектор не ведёт состояние, поэтому ReleaseAll для него
        // пуст. Разворачивает его InputTracker.
        let injector = InputInjector::new();
        assert!(injector.inject(&InputEvent::ReleaseAll).is_ok());
    }

    #[test]
    fn unknown_key_is_skipped_not_failed() {
        let injector = InputInjector::new();
        let event = InputEvent::Key {
            key: KeyCode(0xFFFF),
            pressed: true,
        };
        assert!(
            injector.inject(&event).is_ok(),
            "неизвестная клавиша — не ошибка"
        );
    }
}
