//! Locating, verifying and fetching the model artefacts.
//!
//! The bundle ships without weights: they are ~170 MB, they are not source, and
//! `models/` is gitignored, so a fresh clone would otherwise build a binary with
//! no model to load and no way to make one. A pinned URL fixes both.
//!
//! The catalogue itself lives in [`stemd_models`], because the iOS app loads
//! the same files and must agree about their names and digests. What stays here
//! is the desktop answer to where they land.
//!
//! # Where they land
//!
//! Not next to the executable. On macOS the bundle's contents are covered by the
//! code signature and writing into them invalidates it; on Windows and Linux the
//! install directory is not writable by the user who runs the program. So they
//! land in the per-user data directory: see [`support_dir`] for where that is
//! on each platform.
//!
//! # Layering
//!
//! ```text
//! preset     the catalogue: which artefacts exist and their pinned digests
//! integrity  hashing what is on disk against those digests
//! download   fetching what is missing
//! ```
//!
//! The last three are `stemd_models`; this module adds the directory they land
//! in and the search order around them.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub use stemd_models::{DEFAULT_PRESET, Preset, RETIRED, WEIGHTS_EXTENSION};
pub use stemd_models::{fetch, is_complete, locate, verify};

// One pinned file at a time, and the type describing one. Only `cuda` wants
// either: everything else fetches a whole `ModelSource`. Gated rather than
// always exported, so the platform that cannot install CUDA does not carry two
// names nothing there can use.
#[cfg(windows)]
pub use stemd_models::fetch_one as fetch_file;
#[cfg(windows)]
pub use stemd_models::RemoteFile;

/// The application's data directory, created if absent.
///
/// ```text
/// macOS    ~/Library/Application Support/stemd
/// Linux    ~/.local/share/stemd             ($XDG_DATA_HOME)
/// Windows  %LOCALAPPDATA%\stemd\data
/// ```
///
/// Everything the server keeps that a user would not want swept: the model
/// artefacts and the settings file. Separated stems are the other half, and they
/// live under the cache root instead, because losing them costs a separation and
/// nothing else.
///
/// Local rather than roaming on Windows: `dirs::data_local_dir`, not
/// `dirs::data_dir`. A roaming profile is copied to and from the domain
/// controller at every logon, and 170 MB of weights that any machine can
/// re-download is precisely what must not be in one. The extra `data` component
/// is the other half of the split described on [`crate::cache::default_dir`]:
/// local app data is also where the cache lands, and the cache root is emptied
/// at every start.
pub fn support_dir() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .context("no data directory for this platform")?
        .join("stemd");
    let dir = if cfg!(windows) { dir.join("data") } else { dir };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `support_dir()/models`, created if absent.
pub fn cache_dir() -> Result<PathBuf> {
    let dir = support_dir()?.join("models");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}
