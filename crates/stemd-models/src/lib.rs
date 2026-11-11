//! The model catalogue, and the machinery for getting it onto a machine.
//!
//! Shared by the desktop server and the iOS app. The catalogue and its pinned
//! digests are the one thing that must not be duplicated: `model_id` is derived
//! from them, so two copies that drift would let a client serve stems from one
//! set of weights as though they came from the other.
//!
//! ```text
//! preset     the catalogue: which artefacts exist and their pinned digests
//! integrity  hashing what is on disk against those digests
//! download   fetching what is missing
//! ```
//!
//! Where the files land is the caller's business. The desktop server puts them
//! in the per-user data directory; an iOS app puts them in its container.

mod download;
mod integrity;
mod preset;

use std::path::{Path, PathBuf};

pub use download::{fetch, fetch_one};
pub use integrity::verify;
pub use preset::{DEFAULT_PRESET, ModelSource, Preset, RETIRED, WEIGHTS_EXTENSION};

// One pinned file at a time, and the type describing one. Only the CUDA path
// wants either: everything else fetches a whole `ModelSource`.
pub use preset::RemoteFile;

/// Bytes hashed or copied per read. Large enough that a 675 MB artefact is not
/// resident, small enough to stay in cache.
pub(crate) const READ_CHUNK: usize = 1 << 20;

/// Where a preset's artefact is, if it is anywhere.
///
/// The first directory that holds every file of the preset's source, so a
/// caller's search order decides which copy wins.
pub fn locate(dirs: &[PathBuf], preset: Preset) -> Option<PathBuf> {
    dirs.iter()
        .find(|d| is_complete(d, preset.source()))
        .cloned()
}

/// True when every file of `source` is present in `dir`.
///
/// Presence only: see [`verify`] for whether the contents are right.
pub fn is_complete(dir: &Path, source: &ModelSource) -> bool {
    source.files.iter().all(|f| dir.join(f.name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completeness_needs_every_file() {
        let dir = std::env::temp_dir().join(format!("stemd-models-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        // Named from the preset rather than spelled out, so retiring or
        // renaming one moves this test with it instead of breaking it.
        let source = Preset::Fast.source();

        assert!(!is_complete(&dir, source));
        for file in source.files {
            std::fs::write(dir.join(file.name), b"x").unwrap();
        }
        assert!(is_complete(&dir, source));

        // A directory holding another preset's artefact is not this one's.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        for file in Preset::Balanced.source().files {
            std::fs::write(dir.join(file.name), b"x").unwrap();
        }
        assert!(!is_complete(&dir, source));

        std::fs::remove_dir_all(&dir).ok();
    }
}
