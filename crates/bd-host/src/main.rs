//! Агент хоста: захват, энкод, инжект ввода.
//!
//! На этапах 1-4 это обычное приложение. Служба Windows появляется
//! на этапе 8 отдельным бинарём (CLAUDE.md §3.2) — не усложнять раньше.

#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("bd-host: каркас, пайплайн — этап 1");
    Ok(())
}
