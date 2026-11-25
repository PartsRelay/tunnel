use std::io::IsTerminal;

use clap::ValueEnum;
use tracing::Level;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Trace => Level::TRACE,
        }
    }
}

/// Logs to stderr. No `log` bridge: it would trace headers.
pub fn init(level: LogLevel, format: LogFormat) {
    let builder = tracing_subscriber::fmt()
        .with_max_level(Level::from(level))
        .with_writer(std::io::stderr);

    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.with_ansi(std::io::stderr().is_terminal()).init(),
    }
}
