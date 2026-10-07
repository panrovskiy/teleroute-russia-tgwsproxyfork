use crate::config::AppConfig;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, util::SubscriberInitExt};

pub fn init(config: &AppConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(AppConfig::logs_dir())?;
    let file: RollingFileAppender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("tele-route")
        .filename_suffix(".log")
        .max_log_files(config.logging.keep_files)
        .build(AppConfig::logs_dir())?;
    let filter = EnvFilter::try_new(&config.logging.level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_ansi(false))
        .with(fmt::layer().with_writer(file).with_ansi(false))
        .try_init()?;
    Ok(())
}
