//! Append-only file logger for the TUI client.
//!
//! The TUI deliberately leaves `tracing` uninitialized so log output doesn't
//! scribble over ratatui's alt-screen. When we need breadcrumbs from the
//! share/view pipeline, this writes timestamped lines to `/tmp/dusk-debug.log`
//! instead. `tail -f /tmp/dusk-debug.log` is the intended observation tool.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

pub fn log(msg: impl std::fmt::Display) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/dusk-debug.log")
    else {
        return;
    };
    let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
    let _ = writeln!(f, "[{ts}] {msg}");
}
