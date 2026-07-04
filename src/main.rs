use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use yuque_backup::{
    cli::{Cli, run},
    config::AppConfig,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load(&cli)?;
    init_tracing(&config)?;
    run(cli, config).await
}

fn init_tracing(config: &AppConfig) -> Result<()> {
    std::fs::create_dir_all(config.state_dir())?;
    let file = tracing_appender::rolling::daily(config.state_dir(), "yuque-backup.log");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(file)
        .with_ansi(false)
        .try_init()
        .ok();
    Ok(())
}
