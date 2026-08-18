use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize file-only application logging.
///
/// Logs rotate daily. `ALAN_LOG_DIR` overrides the default `~/.alan/logs` path.
/// `ALAN_LOG` controls filtering; `RUST_LOG` remains supported as fallback.
pub fn init() -> Result<WorkerGuard> {
    let directory = log_directory()?;
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create log directory {}", directory.display()))?;

    let file_appender = tracing_appender::rolling::daily(&directory, "debug.log");
    let (writer, guard) = tracing_appender::non_blocking(file_appender);
    let filter = EnvFilter::try_from_env("ALAN_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| {
            EnvFilter::new("info,alan=debug,agent=debug,llm=debug,providers=debug,tools=debug")
        });
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true);

    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init()
        .context("failed to initialize logging subscriber")?;

    tracing::info!(log_directory = %directory.display(), "logging initialized");
    Ok(guard)
}

fn log_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ALAN_LOG_DIR") {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .context("cannot determine Alan home directory for logs")?;
    Ok(PathBuf::from(home).join(".alan").join("logs"))
}
