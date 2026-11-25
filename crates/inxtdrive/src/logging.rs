use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::paths;

/// Logs to daily files under the data folder. Never logs credentials.
pub fn init_file(level: &str) -> Option<WorkerGuard> {
    let dir = paths::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("inxtdrive")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("INXTDRIVE_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,inxtdrive={level},inxt_core={level},inxt_api={level},inxt_winfsp={level},inxt_fuse={level}")));
    tracing_subscriber::fmt().with_writer(writer).with_ansi(false).with_env_filter(filter).init();
    Some(guard)
}

pub fn init_console(level: &str) {
    let filter = EnvFilter::try_from_env("INXTDRIVE_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,inxtdrive={level},inxt_core={level},inxt_api={level},inxt_winfsp={level},inxt_fuse={level}")));
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(filter).init();
}
