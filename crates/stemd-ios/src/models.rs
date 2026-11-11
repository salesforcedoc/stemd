//! Getting the weights into the app container.
//!
//! The bundle does not carry ~170 MB of weights, and a first run that fails
//! because they are absent would leave the app with no way to become useful.
//! The catalogue and the downloader are `stemd_models`, shared with the desktop;
//! this module adds the thread and the progress the app polls.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use stemd_models::Preset;

pub struct Models {
    dir: PathBuf,
    preset: Preset,
    state: Mutex<State>,
}

#[derive(Clone, Default)]
struct State {
    downloading: bool,
    file: Option<String>,
    done: u64,
    total: u64,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    /// Every artefact the preset runs is on disk.
    pub present: bool,
    pub downloading: bool,
    pub file: Option<String>,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

impl Models {
    pub fn new(dir: PathBuf, preset: Preset) -> Arc<Self> {
        Arc::new(Self {
            dir,
            preset,
            state: Mutex::new(State::default()),
        })
    }

    pub fn status(&self) -> Status {
        let state = self.state.lock().clone();
        Status {
            present: self.artefacts_present(),
            downloading: state.downloading,
            file: state.file,
            done: state.done,
            total: state.total,
            error: state.error,
        }
    }

    /// True when every file the preset loads is present.
    ///
    /// Presence, not integrity: the download verifies as it writes, and the
    /// model itself refuses a file whose tensors disagree. Hashing 672 MB on a
    /// status poll would cost more than the answer is worth.
    pub fn artefacts_present(&self) -> bool {
        self.preset
            .artefacts()
            .iter()
            .all(|a| self.dir.join(format!("{a}.safetensors")).is_file())
    }

    /// Start a fetch in the background. False when one is already running.
    pub fn start_download(self: &Arc<Self>) -> bool {
        {
            let mut state = self.state.lock();
            if state.downloading {
                return false;
            }
            *state = State {
                downloading: true,
                ..State::default()
            };
        }

        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("stemd-ios-models".to_owned())
            .spawn(move || this.run_download());
        if let Err(err) = spawned {
            let mut state = self.state.lock();
            state.downloading = false;
            state.error = Some(format!("starting the download thread: {err}"));
            return false;
        }
        true
    }

    fn run_download(self: Arc<Self>) {
        let outcome = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            // Files already present are skipped, so a retry after a lost
            // connection is a resume for the files that made it and a restart
            // for the one that did not.
            stemd_models::fetch(&self.dir, self.preset.source(), |file, done, total| {
                let mut state = self.state.lock();
                state.file = Some(file.to_owned());
                state.done = done;
                state.total = total;
            })?;
            Ok(())
        })();

        let mut state = self.state.lock();
        state.downloading = false;
        state.file = None;
        match outcome {
            Ok(()) => state.error = None,
            Err(err) => state.error = Some(format!("{err:#}")),
        }
    }
}
