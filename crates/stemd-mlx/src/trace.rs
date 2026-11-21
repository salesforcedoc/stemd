//! A line-oriented trace for the separation path.
//!
//! MLX failures on a device can end the process before anything can be logged
//! through the normal runtime: no crash report, no stderr, just a gone app. A
//! file that is flushed as each phase starts survives that, and the last line
//! names the phase. Dormant until a path is installed, so the desktop and the
//! unit tests pay nothing.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Send [`note`] to `path`. The first call wins; later ones are ignored so a
/// second server cannot redirect the first one's trace.
pub fn set(path: PathBuf) {
    let _ = PATH.set(path);
}

/// Append one line, including what MLX is holding. A missing path is a no-op.
pub fn note(label: &str) {
    let Some(path) = PATH.get() else { return };
    let line = format!(
        "{:.3} {label} active={} cached={} peak={}\n",
        now(),
        crate::memory::active(),
        crate::memory::cached(),
        crate::memory::peak(),
    );
    write(path, &line);
}

/// Append one line without asking MLX what it holds.
///
/// For the panic hook: a panic can fire inside the allocator, and querying it
/// from the hook is how the logging path becomes a second panic.
pub fn line(label: &str) {
    let Some(path) = PATH.get() else { return };
    write(path, &format!("{:.3} {label}\n", now()));
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn write(path: &std::path::Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
        let _ = file.sync_data();
    }
}
