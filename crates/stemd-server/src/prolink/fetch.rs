//! The one worker that turns a seen track into a local file.
//!
//! Downloads are serialised: an eager monitor that fetched every track a DJ
//! browsed would saturate the link and fill the separation queue. Each request
//! reuses a parsed `export.pdb` for its medium until that database is stale or
//! the requested track is missing from it.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;

use super::identity::{self, Entry, Index, TrackKey};
use super::nfs::{self, NfsClient};
use super::packets::Slot;
use super::pdb::{Pdb, Track};
use super::{Downloaded, FetchKey, FetchState, MetaView, State};
use crate::details::TrackDetails;

/// How long a parsed database is trusted. A medium can be swapped; a track ID
/// missing from the cache is the other signal to reload.
const PDB_TTL: Duration = Duration::from_secs(300);

/// Do not lock the shared state for every 8 KB chunk.
const PROGRESS_STEP: u64 = 256 * 1024;

/// One track to fetch. The address is the source player's, resolved by the
/// monitor from its keepalives.
pub(super) struct Request {
    pub key: FetchKey,
    pub address: Ipv4Addr,
}

/// What the monitor and the window ask the download worker to do.
pub(super) enum Command {
    Fetch(Request),
    /// Drop every file fetched from a deck and the identities that map them,
    /// so loading the same track again downloads and separates it again.
    Forget,
}

pub(super) struct Fetcher {
    state: Arc<Mutex<State>>,
    directory: PathBuf,
    sink: Sender<Downloaded>,
    index: Index,
    databases: HashMap<(u8, Slot), (Instant, Arc<Pdb>)>,
    /// Off when the monitor is display-only: metadata is still resolved for the
    /// window, but no audio is fetched.
    download: bool,
}

impl Fetcher {
    pub fn new(
        state: Arc<Mutex<State>>,
        directory: PathBuf,
        sink: Sender<Downloaded>,
        download: bool,
    ) -> Self {
        remove_partials(&directory);
        let index = Index::open(&directory);
        Self {
            state,
            directory,
            sink,
            index,
            databases: HashMap::new(),
            download,
        }
    }

    pub fn run(mut self, commands: Receiver<Command>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            match commands.recv_timeout(Duration::from_millis(300)) {
                Ok(Command::Fetch(request)) => self.handle(request),
                Ok(Command::Forget) => self.forget(),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Empty the download directory and forget the index. Separated stems live
    /// in the cache, not here; this only drops the copies fetched from decks.
    fn forget(&mut self) {
        match std::fs::read_dir(&self.directory) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let removed = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(err) = removed {
                        tracing::warn!("ProLink: removing {}: {err}", path.display());
                    }
                }
            }
            Err(err) => tracing::warn!("ProLink: reading {}: {err}", self.directory.display()),
        }
        self.index = Index::open(&self.directory);
        tracing::info!(
            "ProLink: forgot downloaded tracks in {}",
            self.directory.display()
        );
    }

    fn handle(&mut self, request: Request) {
        let key = request.key;
        if let Err(err) = self.try_handle(&request) {
            if self.download {
                tracing::warn!(
                    "ProLink: track {} from deck {}: {err:#}",
                    key.id,
                    key.source
                );
                mark(&self.state, key, FetchState::Failed(format!("{err:#}")));
            } else {
                // Display-only: a deck whose medium cannot be read still has a
                // track ID and a play state worth showing.
                tracing::debug!(
                    "ProLink: no metadata for track {} from deck {}: {err:#}",
                    key.id,
                    key.source
                );
                mark(&self.state, key, FetchState::Unavailable);
            }
        }
    }

    fn try_handle(&mut self, request: &Request) -> Result<()> {
        let mut client = NfsClient::connect(request.address)?;
        let mounted = client.mount_slot(request.key.slot)?;
        let outcome = self.fetch(&mut client, &mounted, request);
        client.unmount(&mounted);
        outcome
    }

    fn fetch(
        &mut self,
        client: &mut NfsClient,
        mounted: &nfs::Mount,
        request: &Request,
    ) -> Result<()> {
        let key = request.key;
        let database = self.database(client, mounted, key)?;
        let track = database
            .track(key.id)
            .cloned()
            .with_context(|| format!("track {} is not in export.pdb", key.id))?;
        let artist = database.artist(track.artist_id).to_owned();
        let album = database.album(track.album_id).to_owned();
        let metadata = MetaView {
            title: track.title.clone(),
            artist: artist.clone(),
        };
        self.state.lock().metas.insert(key, metadata.clone());

        // Display-only: the title and artist are the whole job.
        if !self.download {
            mark(&self.state, key, FetchState::Idle);
            return Ok(());
        }

        let hash = identity::metadata_hash(&track.title, &artist, &album, track.file_size);
        let full = TrackKey {
            source: key.source,
            slot: key.slot,
            id: key.id,
            hash,
        };

        if let Some(entry) = self.index.get(&full) {
            let path = entry.path.clone();
            if path.is_file() {
                mark(&self.state, key, FetchState::Seen);
                return Ok(());
            }
            self.index.remove(&full)?;
        }

        if track.path.is_empty() {
            bail!("track {} has no file path in export.pdb", key.id);
        }
        let mut file = client.open(mounted, &track.path)?;
        if file.size() == 0 {
            // A few media report the size only in the PDB.
            file.set_size(track.file_size as u64);
        }
        if file.size() == 0 {
            bail!("the deck reports no size for {}", track.path);
        }

        let destination = self.destination(&track, &full);
        if std::fs::metadata(&destination).is_ok_and(|metadata| metadata.len() == file.size()) {
            self.record(&full, &destination, &metadata);
            mark(&self.state, key, FetchState::Seen);
            return Ok(());
        }

        let details = TrackDetails {
            title: Some(track.title.clone()),
            artist: Some(artist.clone()),
            album: Some(album.clone()),
            source: Some("prolink".to_owned()),
            local_file: Some(destination.clone()),
            medium_path: Some(PathBuf::from(&track.path)),
            deck: Some(key.source),
            slot: Some(key.slot.as_u8()),
            track_id: Some(key.id),
        };

        let part = destination.with_file_name(format!(
            "{}.part",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("track")
        ));
        mark(
            &self.state,
            key,
            FetchState::Downloading {
                done: 0,
                total: file.size(),
            },
        );

        let shared = Arc::clone(&self.state);
        let mut written = 0u64;
        let downloaded = client.download_to(&file, &part, |done, total| {
            if done == total || done - written >= PROGRESS_STEP {
                written = done;
                shared
                    .lock()
                    .fetches
                    .insert(key, FetchState::Downloading { done, total });
            }
        });
        if let Err(err) = downloaded {
            let _ = std::fs::remove_file(&part);
            return Err(err);
        }

        let length = std::fs::metadata(&part)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if length != file.size() {
            let _ = std::fs::remove_file(&part);
            bail!(
                "{} expected {} bytes but received {length}",
                track.path,
                file.size()
            );
        }
        let _ = std::fs::remove_file(&destination);
        std::fs::rename(&part, &destination)
            .with_context(|| format!("moving {} into place", destination.display()))?;

        self.record(&full, &destination, &metadata);
        mark(&self.state, key, FetchState::Downloaded);
        if let Err(err) = self.sink.send(Downloaded {
            path: destination.clone(),
            details,
        }) {
            tracing::warn!(
                "ProLink: downloaded {} but could not hand it to the separator: {err}",
                destination.display()
            );
        }
        tracing::info!(
            "ProLink: downloaded {} (track {} from deck {}) to {}",
            metadata.title,
            key.id,
            key.source,
            destination.display()
        );
        Ok(())
    }

    /// One parsed database per medium, refreshed when stale or when the track
    /// is missing from it.
    fn database(
        &mut self,
        client: &mut NfsClient,
        mounted: &nfs::Mount,
        key: FetchKey,
    ) -> Result<Arc<Pdb>> {
        let cache = (key.source, key.slot);
        if let Some((at, database)) = self.databases.get(&cache)
            && at.elapsed() < PDB_TTL
            && database.track(key.id).is_some()
        {
            return Ok(Arc::clone(database));
        }

        let bytes = match client.read_path(mounted, "/PIONEER/rekordbox/export.pdb") {
            Ok(bytes) => bytes,
            Err(err) if nfs::is_noent(&err) => {
                client.read_path(mounted, "/.PIONEER/rekordbox/export.pdb")?
            }
            Err(err) => return Err(err),
        };
        let database = Arc::new(Pdb::parse(&bytes)?);
        tracing::debug!(
            "ProLink: read export.pdb from deck {} slot {} ({} tracks)",
            key.source,
            key.slot.name(),
            database.track_count()
        );
        self.databases
            .insert(cache, (Instant::now(), Arc::clone(&database)));
        Ok(database)
    }

    fn destination(&self, track: &Track, key: &TrackKey) -> PathBuf {
        let title = crate::ident::portable(&track.title);
        let title: String = title.chars().take(80).collect();
        let title = if title.trim_matches('_').is_empty() {
            "track".to_owned()
        } else {
            title
        };
        let extension = Path::new(&track.path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(crate::ident::portable)
            .filter(|extension| !extension.is_empty())
            .unwrap_or_else(|| "bin".to_owned());
        let extension: String = extension.chars().take(8).collect();
        self.directory.join(format!(
            "{title}-{}-{}.{extension}",
            key.id,
            key.short_hash()
        ))
    }

    fn record(&mut self, key: &TrackKey, path: &Path, metadata: &MetaView) {
        if let Err(err) = self.index.insert(Entry {
            source: key.source,
            slot: key.slot.as_u8(),
            id: key.id,
            hash: key.hash.clone(),
            path: path.to_path_buf(),
            title: metadata.title.clone(),
        }) {
            // The file is on disk; a failed index write costs one re-download.
            tracing::warn!("ProLink: could not record {}: {err:#}", path.display());
        }
    }
}

fn mark(state: &Mutex<State>, key: FetchKey, fetch: FetchState) {
    state.lock().fetches.insert(key, fetch);
}

/// Remove halves of downloads that a quit interrupted.
fn remove_partials(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("part") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_destination_is_a_single_safe_component() {
        let directory = PathBuf::from("/tmp/prolink");
        let fetcher = Fetcher {
            state: Arc::new(Mutex::new(State::default())),
            directory: directory.clone(),
            sink: std::sync::mpsc::channel().0,
            index: Index::open(&directory),
            databases: HashMap::new(),
            download: true,
        };
        let key = TrackKey {
            source: 1,
            slot: Slot::Usb,
            id: 9,
            hash: identity::metadata_hash("T", "A", "L", 1),
        };
        let track = Track {
            id: 9,
            title: "../../evil / title".into(),
            artist_id: 0,
            album_id: 0,
            file_size: 1,
            duration: 1,
            path: "/Contents/a.flac".into(),
            filename: "a.flac".into(),
        };
        let destination = fetcher.destination(&track, &key);
        assert_eq!(destination.parent(), Some(directory.as_path()));
        let name = destination.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains('/'));
        assert!(name.contains(&format!("-9-{}", key.short_hash())));
        assert!(name.ends_with(".flac"));
    }
}
