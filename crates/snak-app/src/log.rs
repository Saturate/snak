//! Logging to ~/Library/Logs/Snak/snak.log, where Console.app also finds it.
//! Never log tokens or message text; URLs are logged without their query.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt as _;

const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub fn path() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join("Library/Logs/Snak/snak.log"))
}

fn open() -> Option<File> {
    let path = path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    // One previous file is kept; enough to see what led up to a crash.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Level comes from SNAK_LOG (e.g. `debug`, `snak_teams=trace`), default info.
pub fn init() {
    let filter = EnvFilter::try_from_env("SNAK_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,naga=warn,wgpu=warn"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);
    // A plain locked file rather than a background writer: after a panic
    // aborts the process, buffered lines would be lost exactly when they matter.
    match open() {
        Some(file) => builder
            .with_ansi(false)
            .with_writer(Mutex::new(file).and(std::io::stderr))
            .init(),
        None => builder.init(),
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        tracing::error!(
            target: "snak::panic",
            %location,
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "panic: {message}"
        );
        default_hook(info);
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Snak starting");
}

/// For logging URLs without their query string, which can carry signatures.
pub fn redact_url(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}
