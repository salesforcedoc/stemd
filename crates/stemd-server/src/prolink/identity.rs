//! What makes a track "the same track", and the record of what was downloaded.
//!
//! The identity is `(source player, slot, track id, metadata hash)`. The hash
//! covers the PDB metadata — title, artist, album and file size — rather than
//! the audio, so deciding whether a track is new does not require reading it
//! first. Two files with identical metadata share an identity by design.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::packets::Slot;

/// A track seen on a medium, independent of which deck is playing it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrackKey {
    pub source: u8,
    pub slot: Slot,
    pub id: u32,
    pub hash: String,
}

impl TrackKey {
    /// The leading eight hex characters, used in a file name.
    pub fn short_hash(&self) -> &str {
        self.hash.get(..8).unwrap_or(&self.hash)
    }
}

/// A fixed-length digest over the fields that decide a track's identity.
pub fn metadata_hash(title: &str, artist: &str, album: &str, file_size: u32) -> String {
    let mut digest = Sha256::new();
    digest.update(b"stemd-prolink-v1\0");
    for field in [title.as_bytes(), artist.as_bytes(), album.as_bytes()] {
        digest.update((field.len() as u32).to_be_bytes());
        digest.update(field);
    }
    digest.update(file_size.to_be_bytes());
    format!("{:x}", digest.finalize())
}

/// One downloaded track, as it is remembered across launches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub source: u8,
    pub slot: u8,
    pub id: u32,
    pub hash: String,
    pub path: PathBuf,
    pub title: String,
}

impl Entry {
    fn key(&self) -> (u8, u8, u32, &str) {
        (self.source, self.slot, self.id, self.hash.as_str())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    entries: Vec<Entry>,
}

/// The `index.json` beside the downloads.
pub struct Index {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl Index {
    /// Read the index, dropping entries whose file has been deleted.
    ///
    /// A damaged index is not a reason to refuse to run: it is a cache of "do
    /// not download this again", and starting empty costs one download.
    pub fn open(directory: &Path) -> Self {
        let path = directory.join("index.json");
        let mut entries = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                Ok(file) => file.entries,
                Err(err) => {
                    tracing::warn!(
                        "ProLink: ignoring {} because it is not readable: {err}",
                        path.display()
                    );
                    Vec::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                tracing::warn!("ProLink: reading {}: {err}", path.display());
                Vec::new()
            }
        };
        entries.retain(|entry| entry.path.is_file());
        Self { path, entries }
    }

    pub fn get(&self, key: &TrackKey) -> Option<&Entry> {
        let wanted = (key.source, key.slot.as_u8(), key.id, key.hash.as_str());
        self.entries.iter().find(|entry| entry.key() == wanted)
    }

    /// Record a downloaded track and write the index.
    pub fn insert(&mut self, entry: Entry) -> Result<()> {
        self.entries.retain(|other| other.key() != entry.key());
        self.entries.push(entry);
        self.save()
    }

    /// Forget a track whose file has gone, so it is downloaded again.
    pub fn remove(&mut self, key: &TrackKey) -> Result<()> {
        let wanted = (key.source, key.slot.as_u8(), key.id, key.hash.as_str());
        self.entries.retain(|entry| entry.key() != wanted);
        self.save()
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&File {
            entries: self.entries.clone(),
        })
        .context("encoding the ProLink index")?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        if let Err(err) = std::fs::rename(&temporary, &self.path) {
            // Windows will not replace an existing file with rename.
            let _ = std::fs::remove_file(&self.path);
            std::fs::rename(&temporary, &self.path)
                .with_context(|| format!("replacing {} after {err}", self.path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stemd-prolink-index-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        dir
    }

    #[test]
    fn the_hash_is_stable_and_separates_its_fields() {
        let a = metadata_hash("Title", "Artist", "Album", 100);
        let b = metadata_hash("Title", "Artist", "Album", 100);
        assert_eq!(a, b);
        assert_ne!(a, metadata_hash("Title", "Artist", "Album", 101));
        // Without length prefixes, "ab"+"c" and "a"+"bc" would collide.
        assert_ne!(
            metadata_hash("ab", "c", "", 1),
            metadata_hash("a", "bc", "", 1)
        );
    }

    #[test]
    fn an_entry_survives_a_reopen_and_a_deleted_file_does_not() {
        let dir = temporary_directory();
        let kept = dir.join("kept.mp3");
        let lost = dir.join("lost.mp3");
        std::fs::write(&kept, b"x").expect("a file");
        std::fs::write(&lost, b"x").expect("a file");

        let kept_key = TrackKey {
            source: 1,
            slot: Slot::Usb,
            id: 10,
            hash: metadata_hash("Kept", "", "", 1),
        };
        let lost_key = TrackKey {
            source: 2,
            slot: Slot::Sd,
            id: 20,
            hash: metadata_hash("Lost", "", "", 1),
        };
        {
            let mut index = Index::open(&dir);
            index
                .insert(Entry {
                    source: kept_key.source,
                    slot: kept_key.slot.as_u8(),
                    id: kept_key.id,
                    hash: kept_key.hash.clone(),
                    path: kept.clone(),
                    title: "Kept".into(),
                })
                .expect("an insert");
            index
                .insert(Entry {
                    source: lost_key.source,
                    slot: lost_key.slot.as_u8(),
                    id: lost_key.id,
                    hash: lost_key.hash.clone(),
                    path: lost.clone(),
                    title: "Lost".into(),
                })
                .expect("an insert");
        }
        std::fs::remove_file(&lost).expect("delete the file");

        let index = Index::open(&dir);
        assert!(index.get(&kept_key).is_some());
        assert!(index.get(&lost_key).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_track_is_found_by_every_part_of_its_identity() {
        let dir = temporary_directory();
        let path = dir.join("track.mp3");
        std::fs::write(&path, b"x").expect("a file");
        let key = TrackKey {
            source: 3,
            slot: Slot::Sd,
            id: 77,
            hash: metadata_hash("T", "A", "L", 9),
        };
        let mut index = Index::open(&dir);
        index
            .insert(Entry {
                source: 3,
                slot: 2,
                id: 77,
                hash: key.hash.clone(),
                path,
                title: "T".into(),
            })
            .expect("an insert");
        assert!(index.get(&key).is_some());
        assert!(
            index
                .get(&TrackKey {
                    hash: metadata_hash("different", "A", "L", 9),
                    ..key.clone()
                })
                .is_none()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
