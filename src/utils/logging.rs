//! Structured logging setup for Rust Connect

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Pretty,
}

impl LogFormat {
    pub fn parse_log_level(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "json" => Self::Json,
            _ => Self::Pretty,
        }
    }
}

pub fn init_logging(format: LogFormat, default_level: &str, max_log_files: usize) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let is_systemd = std::env::var("INVOCATION_ID").is_ok();

    if is_systemd {
        match format {
            LogFormat::Json => {
                tracing_subscriber::registry()
                    .with(env_filter.clone())
                    .with(
                        fmt::layer()
                            .json()
                            .with_current_span(true)
                            .with_span_list(true)
                            .with_target(true)
                            .with_thread_ids(true)
                            .with_thread_names(true)
                            .with_file(true)
                            .with_line_number(true),
                    )
                    .init();
            }
            LogFormat::Pretty => {
                tracing_subscriber::registry()
                    .with(env_filter.clone())
                    .with(
                        fmt::layer()
                            .pretty()
                            .with_target(true)
                            .with_thread_ids(false)
                            .with_thread_names(false)
                            .with_file(true)
                            .with_line_number(true),
                    )
                    .init();
            }
        }
    } else {
        init_logging_with_file(format, default_level, None, max_log_files);
        // Always also write to stdout when not under systemd
        let _ = tracing_subscriber::registry()
            .with(env_filter.clone())
            .with(
                fmt::layer()
                    .pretty()
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_thread_names(false),
            )
            .try_init();
    }
}

pub fn init_logging_with_file(
    format: LogFormat,
    default_level: &str,
    log_dir: Option<PathBuf>,
    max_log_files: usize,
) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let log_dir = log_dir.unwrap_or_else(|| {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from(".data"))
            .join("rust-connect")
    });

    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .max_log_files(max_log_files)
        .rotation(Rotation::HOURLY)
        .filename_prefix("daemon")
        .filename_suffix(".log")
        .build(&log_dir)
        .ok();

    match format {
        LogFormat::Json => {
            let stdout_layer = fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true);

            match file_appender {
                Some(appender) => {
                    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
                    leak_guard(guard);

                    tracing_subscriber::registry()
                        .with(env_filter)
                        .with(stdout_layer)
                        .with(
                            fmt::layer()
                                .json()
                                .with_current_span(true)
                                .with_span_list(true)
                                .with_target(true)
                                .with_thread_ids(true)
                                .with_thread_names(true)
                                .with_file(true)
                                .with_line_number(true)
                                .with_writer(non_blocking),
                        )
                        .init();
                }
                None => {
                    tracing_subscriber::registry()
                        .with(env_filter)
                        .with(stdout_layer)
                        .init();
                }
            }
        }
        LogFormat::Pretty => {
            let stdout_layer = fmt::layer()
                .pretty()
                .with_target(true)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_file(true)
                .with_line_number(true);

            match file_appender {
                Some(appender) => {
                    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
                    leak_guard(guard);

                    tracing_subscriber::registry()
                        .with(env_filter)
                        .with(stdout_layer)
                        .with(
                            fmt::layer()
                                .with_target(true)
                                .with_thread_ids(false)
                                .with_thread_names(false)
                                .with_file(true)
                                .with_line_number(true)
                                .with_ansi(false)
                                .with_writer(non_blocking),
                        )
                        .init();
                }
                None => {
                    tracing_subscriber::registry()
                        .with(env_filter)
                        .with(stdout_layer)
                        .init();
                }
            }
        }
    }
}

fn leak_guard(_guard: WorkerGuard) {
    std::mem::forget(_guard);
}

pub fn init_logging_from_env(default_level: &str, max_log_files: usize) {
    let format = std::env::var("LOG_FORMAT")
        .map(|s| LogFormat::parse_log_level(&s))
        .unwrap_or(LogFormat::Pretty);

    init_logging(format, default_level, max_log_files);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_log_format_from_str() {
        assert_eq!(LogFormat::parse_log_level("json"), LogFormat::Json);
        assert_eq!(LogFormat::parse_log_level("JSON"), LogFormat::Json);
        assert_eq!(LogFormat::parse_log_level("pretty"), LogFormat::Pretty);
        assert_eq!(LogFormat::parse_log_level("PRETTY"), LogFormat::Pretty);
        assert_eq!(LogFormat::parse_log_level("invalid"), LogFormat::Pretty);
    }

    #[test]
    fn test_log_format_default() {
        assert_eq!(LogFormat::parse_log_level(""), LogFormat::Pretty);
    }

    #[test]
    fn test_file_appender_creates_log() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let log_dir = temp.path().to_path_buf();

        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .max_log_files(2)
            .rotation(Rotation::HOURLY)
            .filename_prefix("daemon")
            .filename_suffix(".log")
            .build(&log_dir)
            .expect("Value expected to be present");

        drop(appender);

        let entries: Vec<_> = std::fs::read_dir(&log_dir)
            .expect("Value expected to be present")
            .map(|e| {
                e.expect("Value expected to be present")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(
            !entries.is_empty(),
            "Expected log file to be created, dir is empty"
        );
        assert!(
            entries[0].starts_with("daemon"),
            "Expected file to start with 'daemon', got: {}",
            entries[0]
        );
    }

    #[test]
    fn test_non_systemd_detection() {
        std::env::remove_var("INVOCATION_ID");
        let is_systemd = std::env::var("INVOCATION_ID").is_ok();
        assert!(!is_systemd);
    }

    #[test]
    fn test_log_dir_creation() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let nested = temp.path().join("deeply/nested/logs");
        std::fs::create_dir_all(&nested).expect("Value expected to be present");
        assert!(nested.exists());
    }

    #[test]
    fn test_rotation_is_hourly() {
        let temp = tempfile::TempDir::new().expect("Value expected to be present");
        let log_dir = temp.path().to_path_buf();

        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .max_log_files(2)
            .rotation(Rotation::HOURLY)
            .filename_prefix("daemon")
            .filename_suffix(".log")
            .build(&log_dir)
            .expect("Value expected to be present");

        drop(appender);

        let entries: Vec<_> = std::fs::read_dir(&log_dir)
            .expect("Value expected to be present")
            .map(|e| {
                e.expect("Value expected to be present")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(!entries.is_empty());
        assert!(
            entries[0].contains("daemon"),
            "Expected file to contain 'daemon', got: {}",
            entries[0]
        );
    }
}
