//! Logging: a rolling daily file, plus stderr when there is one.
//!
//! A tray app has nowhere to print. Every `tracing` call the code already
//! makes (link transitions, failed trigger registrations, meter rebinds) has
//! been going nowhere until now, and "did it heal itself overnight?" is a
//! question only a durable log can answer.
//!
//! Daily rolling rather than size-based: the questions asked of this log are
//! about *when* ("what happened around 3am"), so a file per day is the shape
//! that makes them easy to answer.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Keep this alive for the process's lifetime.
///
/// The appender writes on a background thread; dropping the guard flushes and
/// stops it. Dropping it early - by ignoring the return value - silently loses
/// every line, which is the classic way to end up with an empty log file.
#[must_use = "dropping the guard stops the log being written"]
pub struct LogGuard(#[allow(dead_code)] WorkerGuard);

/// Start logging into `dir`.
///
/// Returns the guard plus the file actually being written, for the UI to show.
/// Failure is not fatal: an app that refuses to start because it cannot open a
/// log file would be worse than one that runs without logs.
pub fn init(dir: &Path) -> (Option<LogGuard>, Option<String>) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return (None, Some(format!("no log directory: {e}")));
    }

    let appender = tracing_appender::rolling::daily(dir, "purptoof.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    // Default to info; RUST_LOG still overrides, which is what anyone
    // debugging will reach for first.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let result = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(writer.and(std::io::stderr))
        .try_init();

    match result {
        Ok(()) => (
            Some(LogGuard(guard)),
            Some(dir.join("purptoof.log").display().to_string()),
        ),
        // Already initialised - the tests do this, and a double-init is not
        // worth failing a launch over.
        Err(e) => (None, Some(format!("logging not started: {e}"))),
    }
}
