//! Opening a folder in the platform's file manager.
//!
//! Shared by the recents rows and the cache control in the status bar. macOS
//! takes the absolute path rather than a bare command: a bundle launched from
//! Finder inherits a minimal PATH, and "the button does nothing" is a bad way to
//! find that out. The other two are resolved through PATH because that is the
//! only place they are.

use std::path::Path;

pub fn show(dir: &Path) {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("/usr/bin/open");
    #[cfg(windows)]
    let mut command = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");

    if let Err(err) = command.arg(dir).spawn() {
        tracing::warn!("could not reveal {}: {err}", dir.display());
    }
}
