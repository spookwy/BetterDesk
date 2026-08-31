//! Клиент: приём, декод, рендер, захват ввода.

#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("bd-client: каркас, пайплайн — этап 1");
    Ok(())
}
