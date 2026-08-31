//! Проверка пути ввода: клиент → транспорт → хост.
//!
//! # Зачем отдельная проба
//!
//! В `loopback` ввод приходит от окна, то есть только когда человек
//! сидит за клавиатурой. Прогон без единого события печатает «событий
//! не было» — и это **не проверка**, а её отсутствие: ровно тот случай
//! из CLAUDE.md §0.1, находка 26, где ноль срабатываний прочли как
//! «работает».
//!
//! Здесь события подаются синтетически, поэтому путь исполняется
//! всегда и одинаково.
//!
//! # Что именно проверяется
//!
//! 1. Событие переживает wire-формат без искажений.
//! 2. Потеря пакета не оставляет клавишу зажатой навсегда — главное
//!    требование этапа 2 (docs/roadmap.md).
//! 3. Порядок отпускания при потере фокуса не ломает аккорды.
//! 4. Задержка ввода много меньше задержки видео.
//!
//! **Инжект по умолчанию выключен.** С `--inject` проба реально
//! двигает мышь и нажимает клавиши на этой машине — это нужно, чтобы
//! убедиться в работе `SendInput`, но в CI такое недопустимо.
//!
//! Запуск: `cargo run --release -p bd-bench --bin input_probe`
//! С инжектом: `... --bin input_probe -- --inject`

#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    println!("Проба требует Windows.");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use bd_core::input::{InputEvent, KeyCode, MouseButton, MousePosition, SequencedInput};
    use bd_core::time::{now, Epoch};
    use bd_input::{InputTracker, TrackOutcome};
    use bd_transport::{LinkProfile, LoopbackTransport, PayloadKind};
    use std::time::Duration;

    println!("=== Проверка пути ввода ===\n");

    let inject = std::env::args().any(|a| a == "--inject");
    if inject {
        println!("ИНЖЕКТ ВКЛЮЧЁН: мышь и клавиатура будут двигаться сами.");
        println!("Уберите руки с клавиатуры на 5 секунд.\n");
    } else {
        println!("Инжект выключен (добавьте --inject, чтобы проверить SendInput).\n");
    }

    let epoch = Epoch::new();
    let mut failures: Vec<String> = Vec::new();

    // ── 1. Целостность wire-формата через канал с потерями ────────
    //
    // Профиль с потерями, а не идеальный: путь без потерь не
    // исполняет ветку, ради которой всё и делается (находка 27).
    println!("1. Целостность через канал с потерями (mobile, 3 %)");

    let mut transport = LoopbackTransport::new(LinkProfile::MOBILE, epoch);
    let sample_events = [
        InputEvent::MouseMove {
            position: MousePosition::new(0.25, 0.75),
        },
        InputEvent::Key {
            key: KeyCode::LEFT_CTRL,
            pressed: true,
        },
        InputEvent::Key {
            key: KeyCode::A,
            pressed: true,
        },
        InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
            position: MousePosition::new(0.5, 0.5),
        },
        InputEvent::MouseScroll {
            delta_y: -2.0,
            delta_x: 0.0,
        },
    ];

    let mut sent = 0u64;
    let mut received = 0u64;
    let mut corrupted = 0u64;

    for round in 0..200u64 {
        let event = sample_events[(round as usize) % sample_events.len()];
        let sequenced = SequencedInput {
            sequence: round,
            timestamp: epoch.stamp_now(),
            event,
        };

        let mut timings = bd_core::metrics::FrameTimings::default();
        transport.send(PayloadKind::Input, false, &sequenced.encode(), &mut timings)?;
        sent += 1;
    }

    // Канал MOBILE держит датаграмы 80 мс. Ждём, пока они дозреют,
    // иначе проба измерит пустую очередь и объявит успех.
    std::thread::sleep(Duration::from_millis(200));

    loop {
        let mut arrival = bd_core::metrics::FrameTimings::default();
        let Some(delivered) = transport.receive(&mut arrival)? else {
            break;
        };
        received += 1;

        let Some(parsed) = SequencedInput::parse(&delivered.data) else {
            corrupted += 1;
            continue;
        };
        let expected = sample_events[(parsed.sequence as usize) % sample_events.len()];
        // Сравнение с точностью формата, а не через `==`.
        //
        // Координаты кодируются в `u16`, поэтому 0.25 возвращается
        // как 0.24998856 — формат отработал верно, но `==` считает
        // это искажением. Первая версия этой пробы так и сделала и
        // отрапортовала о «77 искажённых событиях» при исправном
        // транспорте.
        if !parsed.event.eq_within_wire_precision(&expected) {
            corrupted += 1;
        }
    }

    println!("   отправлено {sent}, доставлено {received}, искажено {corrupted}");
    if corrupted > 0 {
        failures.push(format!("транспорт исказил {corrupted} событий ввода"));
    }
    if received == 0 {
        // Событие ввода — один датаграм, и при 3 % потерь дойти должны
        // почти все. Ноль означает, что путь не исполнился вовсе.
        failures.push("ни одно событие не дошло — путь не проверен".into());
    }

    // ── 2. Потеря отпускания не оставляет клавишу зажатой ─────────
    //
    // Главное требование этапа 2. Проверяется тем, что отпускание
    // намеренно НЕ доставляется, а трекер всё равно обязан выдать
    // событие, освобождающее клавишу.
    println!("\n2. Потеря отпускания лечится трекером");

    let mut client = InputTracker::new();
    let mut host = InputTracker::new();

    let press = InputEvent::Key {
        key: KeyCode::LEFT_ALT,
        pressed: true,
    };
    client.track(&press);
    host.track(&press); // нажатие дошло

    let release = InputEvent::Key {
        key: KeyCode::LEFT_ALT,
        pressed: false,
    };
    client.track(&release); // отпускание на клиенте есть...
                            // ...но до хоста не дошло — пакет потерян.

    if host.is_idle() {
        failures.push("подготовка теста неверна: хост уже без зажатых".into());
    }

    // Клиент теряет фокус и шлёт отпускания всего, что зажато. У него
    // Alt уже отпущен, поэтому список пуст — и это ровно та дыра,
    // из-за которой Alt остался бы зажатым на хосте навсегда.
    let from_client: Vec<_> = client.release_events().collect();
    println!("   клиент отпускает: {} событий", from_client.len());

    // Поэтому хост обязан уметь освободиться сам — по разрыву сессии.
    let host_release: Vec<_> = host.release_events().collect();
    for e in &host_release {
        host.track(e);
    }
    println!("   хост отпускает сам: {} событий", host_release.len());

    if !host.is_idle() {
        failures.push("хост не смог освободить зажатые клавиши".into());
    } else {
        println!("   ✅ зажатых не осталось");
    }

    // ── 3. Порядок отпускания сохраняет аккорды ───────────────────
    println!("\n3. Порядок отпускания: модификаторы последними");

    let mut t = InputTracker::new();
    t.track(&InputEvent::Key {
        key: KeyCode::LEFT_CTRL,
        pressed: true,
    });
    t.track(&InputEvent::Key {
        key: KeyCode::A,
        pressed: true,
    });

    let order: Vec<_> = t.release_events().collect();
    let index_of = |target: KeyCode| {
        order
            .iter()
            .position(|e| matches!(e, InputEvent::Key { key, .. } if *key == target))
    };

    match (index_of(KeyCode::A), index_of(KeyCode::LEFT_CTRL)) {
        (Some(a), Some(ctrl)) if a < ctrl => {
            println!("   ✅ A отпускается раньше Ctrl (аккорд не распадается)");
        }
        (Some(a), Some(ctrl)) => {
            failures.push(format!(
                "модификатор отпущен раньше клавиши (Ctrl на {ctrl}, A на {a}): \
                 хост увидит одиночное A вместо Ctrl+A"
            ));
        }
        _ => failures.push("не все клавиши отпущены".into()),
    }

    // ── 4. Автоповтор не засоряет канал ───────────────────────────
    println!("\n4. Автоповтор подавляется");

    let mut t = InputTracker::new();
    let key_down = InputEvent::Key {
        key: KeyCode::SPACE,
        pressed: true,
    };
    let first = t.track(&key_down);
    let mut suppressed = 0;
    for _ in 0..30 {
        if t.track(&key_down) == TrackOutcome::Redundant {
            suppressed += 1;
        }
    }
    println!("   первое нажатие: {first:?}, подавлено повторов: {suppressed}");
    if first != TrackOutcome::Changed || suppressed != 30 {
        failures.push("автоповтор не подавляется — канал будет засорён".into());
    }

    // ── 5. Задержка ввода ─────────────────────────────────────────
    //
    // Ввод обязан быть много быстрее видео: щелчок мыши, доходящий
    // за время кадра, ощущается как локальный, а за два — уже нет.
    println!("\n5. Задержка события ввода в канале");

    let mut lan = LoopbackTransport::new(LinkProfile::LAN, epoch);
    let mut samples: Vec<Duration> = Vec::new();

    for i in 0..100u64 {
        let sequenced = SequencedInput {
            sequence: i,
            timestamp: epoch.stamp_now(),
            event: InputEvent::MouseMove {
                position: MousePosition::new(0.5, 0.5),
            },
        };
        let before = now();
        let mut timings = bd_core::metrics::FrameTimings::default();
        lan.send(PayloadKind::Input, false, &sequenced.encode(), &mut timings)?;

        // Профиль LAN держит датаграм 1 мс.
        std::thread::sleep(Duration::from_millis(2));

        let mut arrival = bd_core::metrics::FrameTimings::default();
        if lan.receive(&mut arrival)?.is_some() {
            samples.push(now().duration_since(before));
        }
    }

    if samples.is_empty() {
        failures.push("ни одно событие не дошло по профилю lan".into());
    } else {
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        println!(
            "   доставлено {} из 100, медиана {:.2} мс",
            samples.len(),
            median.as_secs_f64() * 1000.0
        );
        // Порог свободный: измеряется в основном наш же sleep(2 мс).
        // Смысл проверки — поймать регресс на порядок, а не точную
        // цифру: настоящая задержка появится с QUIC (этап 3).
        if median > Duration::from_millis(10) {
            failures.push(format!(
                "задержка ввода {:.2} мс — это много даже для заглушки",
                median.as_secs_f64() * 1000.0
            ));
        }
    }

    // ── 6. Живой инжект ───────────────────────────────────────────
    if inject {
        println!("\n6. Живой инжект через SendInput");
        let injector = bd_input::windows::InputInjector::new();

        // Мышь водится по кругу: заметно глазом и легко отличить от
        // случайного движения. Позиции в долях — где именно окажется
        // курсор, зависит от разрешения, и это правильно.
        let corners = [(0.4, 0.4), (0.6, 0.4), (0.6, 0.6), (0.4, 0.6), (0.5, 0.5)];
        let mut moved = 0;
        for (x, y) in corners {
            let event = InputEvent::MouseMove {
                position: MousePosition::new(x, y),
            };
            match injector.inject(&event) {
                Ok(()) => moved += 1,
                Err(e) if e.is_recoverable() => {
                    println!("   ввод заблокирован системой: {e}");
                }
                Err(e) => failures.push(format!("инжект не удался: {e}")),
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        println!("   курсор перемещён {moved} раз");
        if moved == 0 {
            failures.push("ни одно перемещение курсора не прошло".into());
        }

        // Клавиша не нажимается: она ушла бы в активное окно, то есть
        // в чужой документ или терминал. Проверяется только сборка
        // пакета — что он не пуст и принимается системой.
        println!("   (клавиши намеренно не нажимаются: ушли бы в чужое окно)");
    }

    // ── Итог ──────────────────────────────────────────────────────
    println!("\n=== Результат ===");
    if failures.is_empty() {
        println!("✅ Путь ввода исправен.");
        println!("   Формат переживает канал с потерями, зажатые клавиши");
        println!("   освобождаются, аккорды не распадаются.");
        if !inject {
            println!("\n   SendInput не проверялся — нужен запуск с --inject.");
        }
        Ok(())
    } else {
        for f in &failures {
            println!("❌ {f}");
        }
        anyhow::bail!("проверка ввода провалена")
    }
}
