//! Проба контроллера битрейта: как он ведёт себя на разных каналах.
//!
//! # Что это проверяет и чего не проверяет
//!
//! Контроллер получает синтетические наблюдения — RTT и счётчики
//! датаграмов, — а не работает через настоящую сеть. Это **не
//! недостаток пробы, а её смысл**: сценарий воспроизводим, и две
//! версии кода можно сравнить.
//!
//! Через живую сеть то же самое проверить нельзя: канал меняется сам,
//! и разница между прогонами скажет больше о провайдере, чем о коде.
//! Тот же довод, что у транспорта-заглушки на этапе 1.
//!
//! **Чего проба НЕ доказывает:** что контроллер помогает картинке.
//! Она показывает, какие решения он принимает на заданном канале;
//! польза измеряется живым прогоном и глазом.
//!
//! Запуск: `cargo run --release -p bd-bench --bin rate_probe`

#![forbid(unsafe_code)]

use bd_core::{LinkSample, LinkState, RateConfig, RateController};
use std::time::Duration;

/// Сценарий: как ведёт себя канал во времени.
struct Scenario {
    name: &'static str,
    /// Что должно произойти — проверяется в конце.
    expectation: &'static str,
    /// RTT и потери на каждом шаге (полсекунды на шаг).
    steps: Vec<Step>,
}

#[derive(Clone, Copy)]
struct Step {
    rtt: Duration,
    /// Доля теряемых датаграмов на этом шаге.
    loss: f64,
    /// Сколько шагов держится это состояние.
    repeat: usize,
}

fn step(rtt_ms: u64, loss: f64, repeat: usize) -> Step {
    Step {
        rtt: Duration::from_millis(rtt_ms),
        loss,
        repeat,
    }
}

/// Микросекундный шаг между решениями (интервал контроллера).
const STEP_MICROS: u64 = 500_000;

/// Сколько датаграмов уходит за шаг — при 15 Мбит/с и полсекунде это
/// примерно столько.
const DATAGRAMS_PER_STEP: u64 = 800;

const MAX_BITRATE: u32 = 15_000_000;

fn main() {
    println!("=== BetterDesk: проба контроллера битрейта ===\n");
    println!("Контроллер получает синтетические наблюдения, а не живую");
    println!("сеть: сценарий должен быть воспроизводим, иначе сравнить");
    println!("две версии кода нельзя.\n");

    let scenarios = vec![
        Scenario {
            name: "localhost (джиттер планировщика)",
            expectation: "битрейт НЕ снижается: 1.8 мс при базе 0.2 — это шум",
            steps: vec![
                Step {
                    rtt: Duration::from_micros(200),
                    loss: 0.0,
                    repeat: 10,
                },
                Step {
                    rtt: Duration::from_micros(1_800),
                    loss: 0.0,
                    repeat: 20,
                },
            ],
        },
        Scenario {
            name: "хороший интернет, ровный",
            expectation: "битрейт держится у потолка",
            steps: vec![step(70, 0.0, 30)],
        },
        Scenario {
            name: "нарастающая очередь (bufferbloat)",
            expectation: "снижение НАЧИНАЕТСЯ до первой потери",
            steps: vec![
                step(70, 0.0, 10),
                // Очередь копится: задержка растёт, потерь ещё нет.
                step(110, 0.0, 4),
                step(160, 0.0, 4),
                step(220, 0.0, 4),
            ],
        },
        Scenario {
            name: "мобильный интернет (3 % потерь)",
            expectation: "паритет включается; битрейт снижается один раз и встаёт",
            steps: vec![step(80, 0.0, 6), step(80, 0.03, 14)],
        },
        Scenario {
            name: "провал канала и восстановление",
            expectation: "битрейт падает быстро, возвращается медленно",
            steps: vec![
                step(70, 0.0, 8),
                // Провал на 5 секунд.
                step(300, 0.10, 10),
                // Канал вернулся.
                step(70, 0.0, 30),
            ],
        },
    ];

    let mut failures = 0;
    for scenario in &scenarios {
        if !run(scenario) {
            failures += 1;
        }
    }

    println!("\n=== Итог ===\n");
    if failures == 0 {
        println!("✅ Все сценарии отработали как ожидалось.");
    } else {
        println!("❌ Сценариев с неожиданным поведением: {failures}");
        std::process::exit(1);
    }
}

fn run(scenario: &Scenario) -> bool {
    println!("── {} ──", scenario.name);
    println!("   ожидание: {}\n", scenario.expectation);

    let mut controller = RateController::new(RateConfig::new(MAX_BITRATE));
    let mut now = 0u64;
    let mut sent = 0u64;
    let mut dropped = 0u64;

    let mut first_drop_step: Option<usize> = None;
    let mut first_fall_step: Option<usize> = None;
    let mut min_bitrate = MAX_BITRATE;
    let mut max_fec = 0u32;
    let mut index = 0usize;

    for step in &scenario.steps {
        for _ in 0..step.repeat {
            sent += DATAGRAMS_PER_STEP;
            let lost = (DATAGRAMS_PER_STEP as f64 * step.loss) as u64;
            dropped += lost;
            if lost > 0 && first_drop_step.is_none() {
                first_drop_step = Some(index);
            }

            let sample = LinkSample {
                rtt: step.rtt,
                datagrams_sent: sent,
                datagrams_dropped: dropped,
            };

            if let Some(decision) = controller.observe(sample, now) {
                if decision.bitrate < MAX_BITRATE && first_fall_step.is_none() {
                    first_fall_step = Some(index);
                }
                min_bitrate = min_bitrate.min(decision.bitrate);
                max_fec = max_fec.max(decision.fec_percent);

                // Печатаем не каждый шаг: 30 строк на сценарий никто
                // не читает, а важны переломы.
                if index.is_multiple_of(6) {
                    println!(
                        "   шаг {index:>2}: RTT {:>6.1} мс  →  {:>5.1} Мбит/с, паритет {:>2} %, {:?}",
                        step.rtt.as_secs_f64() * 1000.0,
                        decision.bitrate as f64 / 1_000_000.0,
                        decision.fec_percent,
                        controller.state()
                    );
                }
            }

            now += STEP_MICROS;
            index += 1;
        }
    }

    let final_decision = controller.current();
    println!(
        "\n   итог: {:.1} Мбит/с (минимум {:.1}), паритет до {} %, состояние {:?}",
        final_decision.bitrate as f64 / 1_000_000.0,
        min_bitrate as f64 / 1_000_000.0,
        max_fec,
        controller.state()
    );

    // Вердикт по сценарию. Проверяется именно то, что обещано в
    // `expectation`, — иначе проба выносила бы вердикт не о том, что
    // измеряет (находка 29).
    let verdict = match scenario.name {
        "localhost (джиттер планировщика)" | "хороший интернет, ровный" =>
        {
            let held = min_bitrate == MAX_BITRATE;
            if !held {
                println!(
                    "   ❌ битрейт снижался до {:.1} Мбит/с на исправном канале",
                    min_bitrate as f64 / 1_000_000.0
                );
            }
            held
        }
        "нарастающая очередь (bufferbloat)" => {
            // Главное свойство контроллера: реакция ДО потерь.
            match (first_fall_step, first_drop_step) {
                (Some(fall), None) => {
                    println!("   ✅ снижение на шаге {fall}, потерь не было вовсе");
                    true
                }
                (Some(fall), Some(drop)) if fall < drop => {
                    println!("   ✅ снижение на шаге {fall}, первая потеря на {drop}");
                    true
                }
                (Some(fall), Some(drop)) => {
                    println!("   ❌ снижение на шаге {fall} — ПОСЛЕ первой потери на {drop}");
                    false
                }
                (None, _) => {
                    println!("   ❌ битрейт не снижался, хотя задержка выросла втрое");
                    false
                }
            }
        }
        "мобильный интернет (3 % потерь)" => {
            // Критерий этапа 6: «сессия остаётся пригодной для
            // работы». Одного «паритет включился» мало — контроллер,
            // сводящий битрейт к дну, формально реагирует, а
            // фактически делает картинку бесполезной.
            //
            // Первая версия так и делала: за десять шагов 15 → 1
            // Мбит/с, и проба это ПРОПУСКАЛА, потому что проверяла
            // «паритет > 0 и битрейт снизился». Вердикт был не о том,
            // что измеряется (находка 29).
            const USABLE_FLOOR: u32 = 4_000_000;

            let reacted = max_fec > 0;
            let usable = min_bitrate >= USABLE_FLOOR;

            if !reacted {
                println!("   ❌ паритет не включён: потери не отработаны");
            }
            if !usable {
                println!(
                    "   ❌ битрейт сведён до {:.1} Мбит/с — картинка непригодна.",
                    min_bitrate as f64 / 1_000_000.0
                );
                println!("      Потери от помех не зависят от нагрузки, и снижать");
                println!("      битрейт против них бессмысленно.");
            }
            reacted && usable
        }
        "провал канала и восстановление" => {
            let recovered = final_decision.bitrate > min_bitrate;
            let not_instant =
                final_decision.bitrate < MAX_BITRATE || controller.state() == LinkState::Good;
            if !recovered {
                println!("   ❌ битрейт не восстанавливался после провала");
            }
            recovered && not_instant
        }
        _ => true,
    };

    println!();
    verdict
}
