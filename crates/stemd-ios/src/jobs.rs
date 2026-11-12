//! Jobs, their files, and the one thread that runs the model.
//!
//! One separation at a time, on a dedicated OS thread. That is not a shortcut:
//! MLX keeps its streams and allocator per thread, and the desktop server runs
//! every job on the same worker for the same reason. A phone has no use for a
//! deeper queue anyway, so the queue here is the channel's backlog and the API
//! reports each job's position in it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stemd_core::{
    Audio, Cancelled, PcmFormat, Progress, ProgressSink, Separate, Stage, StemFormat, Stems,
    encode_stem,
};
use stemd_models::Preset;

use crate::api::ApiError;
use crate::separator;

/// How often the worker looks up from the queue to notice a stop.
const IDLE_POLL: Duration = Duration::from_millis(300);

/// The extension of an uploaded file name, when it is one that can be a path
/// component. Used as symphonia's format hint, not as a gate: the decoder
/// probes the content either way.
fn plain_extension(filename: &str) -> Option<String> {
    let (_, ext) = filename.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 10 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// The identity of one separation.
///
/// Covers the audio bytes, the output container, the model, and anything else
/// that interprets the same bytes differently — for raw PCM, its format, rate
/// and channel count. Two submissions with the same key are the same result,
/// so the second is answered with the first job rather than separated again.
fn content_key(body: &[u8], format: StemFormat, model: &str, extra: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"stemd-ios-job-v1\0");
    hasher.update((body.len() as u64).to_be_bytes());
    hasher.update(body);
    hasher.update(format.to_string().as_bytes());
    hasher.update(model.as_bytes());
    hasher.update(extra);
    format!("{:x}", hasher.finalize())
}

/// Clean a client-supplied source-file hash.
///
/// The clients use FNV-1a rendered as 16 lowercase hex characters, but the
/// protocol only needs it to be a short opaque token; a value that cannot be a
/// path component or is absurdly long is dropped rather than trusted.
fn normalise_source_hash(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.len() > 64
        || !value.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

/// Bytes the system says this process may still allocate before it is killed.
///
/// iOS 13 and later. Zero when the question does not apply (tests, macOS
/// tooling), which reads as "unknown" in the trace.
#[cfg(target_os = "ios")]
pub(crate) fn available_memory() -> usize {
    // SAFETY: the API has no arguments and no failure mode beyond returning 0
    // on a system that predates it.
    unsafe extern "C" {
        fn os_proc_available_memory() -> usize;
    }
    unsafe { os_proc_available_memory() }
}

#[cfg(not(target_os = "ios"))]
pub(crate) fn available_memory() -> usize {
    0
}

/// Modification time of the running app executable, in Unix seconds.
///
/// The closest thing to a build stamp the bundle has: it moves when the app is
/// linked and installed. Reported in `/v1/health` so an install can be
/// identified from another machine, and shown in the app's Debug screen from
/// the bundle itself.
pub(crate) fn app_binary_unix() -> u64 {
    std::env::current_exe()
        .and_then(|path| std::fs::metadata(path))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|age| age.as_secs())
        .unwrap_or(0)
}

/// Append one line about memory to `debug-memory.log` beside the jobs.
///
/// Written on demand and flushed, because the process it is meant to describe
/// is the one the system is about to kill. Kept in the jobs root so it can be
/// pulled off the device with the container.
pub(crate) fn note(root: &Path, label: &str) {
    use std::io::Write;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let line = format!(
        "{now:.3} {label} avail={} active={} cached={} peak={}\n",
        available_memory(),
        stemd_core::memory::active(),
        stemd_core::memory::cached(),
        stemd_core::memory::peak(),
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("debug-memory.log"))
    {
        let _ = file.write_all(line.as_bytes());
        let _ = file.sync_data();
    }
}

pub struct Options {
    /// The jobs root; each job gets `<root>/<id>/`.
    pub root: PathBuf,
    /// Where the `.safetensors` artefacts live.
    pub models_dir: PathBuf,
    pub preset: Preset,
    pub max_track_secs: f64,
    /// The share token, also written into stem URLs.
    pub token: String,
}

/// Load finished jobs from the jobs root.
///
/// A job's directory is its record: the stems and `detail.json` survive a
/// launch, so the list can be rebuilt without separating anything again. An
/// entry missing its detail or one of its stems is left where it is and skipped
/// — deleting someone's stems on startup is not this function's call.
///
/// Returns the jobs and the highest sequence number seen, so ids for new jobs
/// never collide with loaded ones.
fn load_existing(root: &Path) -> (HashMap<String, Arc<Job>>, u64) {
    let mut jobs = HashMap::new();
    let mut max_created = 0;

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!("reading {}: {err}", root.display());
            return (jobs, max_created);
        }
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir.file_name().and_then(|n| n.to_str()) == Some("incoming") {
            continue;
        }
        let Some(id) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let detail: Detail = match std::fs::read(dir.join("detail.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(detail) => detail,
            None => continue,
        };
        let format: StemFormat = detail.format.parse().unwrap_or(StemFormat::Flac);

        let mut stems = Vec::new();
        let mut complete = true;
        for stem in &detail.stems {
            let path = dir.join(&stem.file);
            if !path.is_file() {
                complete = false;
                break;
            }
            stems.push(Stem {
                name: stem.name.clone(),
                path,
                bytes: stem.bytes,
                gain: stem.gain,
                content_type: format.content_type(),
            });
        }
        if !complete || stems.is_empty() {
            continue;
        }

        let created = id
            .rsplit_once('-')
            .and_then(|(_, seq)| seq.parse::<u64>().ok())
            .unwrap_or(0);
        let channels = if detail.channels > 0 {
            detail.channels
        } else {
            2
        };
        let frames = if detail.frames > 0 {
            detail.frames
        } else {
            (detail.duration_secs * f64::from(detail.sample_rate)).round() as usize
        };
        let realtime = if detail.separation_secs > 0.0 {
            detail.duration_secs / detail.separation_secs
        } else {
            0.0
        };

        let job = Arc::new(Job {
            id: id.clone(),
            key: detail.key,
            source_hash: Mutex::new(detail.source_hash),
            created,
            title: detail.title,
            artist: detail.artist,
            album: detail.album,
            source: Mutex::new(Source::None),
            dir,
            state: Mutex::new(State::Done {
                seconds: detail.separation_secs,
                realtime,
            }),
            cancelled: AtomicBool::new(false),
            stems: Mutex::new(stems),
            result: Mutex::new(Some(Geometry {
                sample_rate: detail.sample_rate,
                channels,
                frames,
                format,
            })),
        });
        max_created = max_created.max(created);
        jobs.insert(id, job);
    }

    (jobs, max_created)
}

pub struct Jobs {
    root: PathBuf,
    preset: Preset,
    max_track_secs: f64,
    token: String,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    tx: Sender<Work>,
    stop: Arc<AtomicBool>,
    seq: AtomicU64,
}

impl Jobs {
    pub fn start(options: Options) -> Result<Arc<Self>> {
        let incoming = options.root.join("incoming");
        std::fs::create_dir_all(&incoming)
            .with_context(|| format!("creating {}", incoming.display()))?;
        // The separation path's own trace, for the failures that take the
        // process down before it can log anywhere else.
        stemd_core::trace::set(options.root.join("debug-memory.log"));

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (loaded, max_created) = load_existing(&options.root);
        let worker_started = Arc::clone(&stop);
        let config = WorkerConfig {
            models_dir: options.models_dir.clone(),
            preset: options.preset,
            max_track_secs: options.max_track_secs,
        };
        std::thread::Builder::new()
            .name("stemd-ios-separate".to_owned())
            .spawn(move || worker(rx, worker_started, config))
            .context("spawning the separation worker")?;

        Ok(Arc::new(Self {
            root: options.root,
            preset: options.preset,
            max_track_secs: options.max_track_secs,
            token: options.token,
            jobs: Mutex::new(loaded),
            tx,
            stop,
            seq: AtomicU64::new(max_created.saturating_add(1)),
        }))
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn preset_label(&self) -> &'static str {
        self.preset.label()
    }

    /// The artefact name, for a health document a desktop client understands.
    pub fn model_name(&self) -> &'static str {
        self.preset.artefact()
    }

    /// A stable identity for cache scoping.
    ///
    /// The desktop derives a digest over the weights and recipe; this build has
    /// one artefact per preset and no switching, so the name is enough to keep
    /// two presets' stems apart. The `-ios` suffix keeps it from colliding with
    /// a desktop cache entry of the same artefact.
    pub fn model_id(&self) -> String {
        format!("{}-ios", self.preset.artefact())
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn max_track_secs(&self) -> f64 {
        self.max_track_secs
    }

    pub fn count(&self) -> usize {
        self.jobs.lock().len()
    }

    /// Write one memory-trace line for this server's jobs root.
    pub fn note(&self, label: &str) {
        note(&self.root, label);
    }

    /// Queue an encoded audio file.
    pub async fn submit_file(
        &self,
        body: axum::body::Bytes,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        format: StemFormat,
        filename: Option<&str>,
        source_hash: Option<&str>,
    ) -> Result<(JobView, bool), ApiError> {
        let source_hash = normalise_source_hash(source_hash);
        if let Some(hash) = source_hash.as_deref() {
            if let Some(job) = self.find_source(hash, format) {
                return Ok((job.view(&self.token), true));
            }
        }
        let digest = Sha256::digest(&body[..]);
        let prefix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        let key = content_key(&body, format, &self.model_id(), &[]);
        if let Some(job) = self.find_reusable(&key, &prefix, format) {
            self.adopt_source_hash(&job, source_hash.as_deref());
            return Ok((job.view(&self.token), true));
        }
        let (id, seq) = self.new_id(&prefix);
        let dir = self.root.join(&id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ApiError::internal(format!("creating {}: {e}", dir.display())))?;
        // Keep the original extension when it is a plain one: symphonia uses it
        // as a hint, and a file named `.audio` would make every format rely on
        // sniffing alone. Sanitised because it becomes a path component.
        let extension = filename
            .and_then(plain_extension)
            .unwrap_or_else(|| "audio".to_owned());
        let input = self.root.join("incoming").join(format!("{id}.{extension}"));
        tokio::fs::write(&input, &body)
            .await
            .map_err(|e| ApiError::internal(format!("writing {}: {e}", input.display())))?;
        self.insert_job(
            id,
            seq,
            key,
            source_hash.unwrap_or_default(),
            Source::File(input),
            title,
            artist,
            album,
            format,
        )
    }

    /// Queue a raw PCM upload in the desktop API's spelling.
    ///
    /// The CDJ sidecar sends this: samples the deck already decoded, at its
    /// own rate and channel count, with no container to unwrap.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_pcm(
        &self,
        body: axum::body::Bytes,
        sample_rate: u32,
        channels: usize,
        pcm: PcmFormat,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        format: StemFormat,
        source_hash: Option<&str>,
    ) -> Result<(JobView, bool), ApiError> {
        let source_hash = normalise_source_hash(source_hash);
        if let Some(hash) = source_hash.as_deref() {
            if let Some(job) = self.find_source(hash, format) {
                return Ok((job.view(&self.token), true));
            }
        }
        // The PCM format, rate and channel count interpret the same bytes
        // differently, so they are part of the key as well.
        let extra = format!("{pcm}\0{sample_rate}\0{channels}");
        let digest = Sha256::digest(&body[..]);
        let prefix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        let key = content_key(&body, format, &self.model_id(), extra.as_bytes());
        if let Some(job) = self.find_reusable(&key, &prefix, format) {
            self.adopt_source_hash(&job, source_hash.as_deref());
            return Ok((job.view(&self.token), true));
        }
        let (id, seq) = self.new_id(&prefix);
        std::fs::create_dir_all(self.root.join(&id))
            .map_err(|e| ApiError::internal(format!("creating the job directory: {e}")))?;
        self.insert_job(
            id,
            seq,
            key,
            source_hash.unwrap_or_default(),
            Source::Pcm {
                bytes: body,
                format: pcm,
                sample_rate,
                channels,
            },
            title,
            artist,
            album,
            format,
        )
    }

    /// The first job this submission can reuse.
    ///
    /// A keyed job must match exactly. Entries written before the content key
    /// existed carry no key; those are matched by the body hash in their id and
    /// the output format, which is the same evidence without the model in it.
    /// The prefix is the first 48 bits of the uploaded body's SHA-256, so a
    /// match still means the same bytes went up.
    fn find_reusable(&self, key: &str, prefix: &str, format: StemFormat) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .values()
            .find(|job| {
                {
                    let state = job.state.lock();
                    if matches!(
                        &*state,
                        State::Failed { .. }
                            | State::Cancelled
                            | State::Queued
                            | State::Running { .. }
                    ) {
                        return false;
                    }
                }
                if job.key == key {
                    return true;
                }
                job.key.is_empty()
                    && job
                        .id
                        .strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with('-'))
                    && job
                        .result
                        .lock()
                        .as_ref()
                        .is_some_and(|geometry| geometry.format == format)
            })
            .cloned()
    }

    fn new_id(&self, prefix: &str) -> (String, u64) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        (format!("{prefix}-{seq}"), seq)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_job(
        &self,
        id: String,
        seq: u64,
        key: String,
        source_hash: String,
        source: Source,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        format: StemFormat,
    ) -> Result<(JobView, bool), ApiError> {
        let dir = self.root.join(&id);
        let job = Arc::new(Job {
            id: id.clone(),
            key,
            source_hash: Mutex::new(source_hash),
            created: seq,
            title,
            artist,
            album,
            source: Mutex::new(source),
            dir,
            state: Mutex::new(State::Queued),
            cancelled: AtomicBool::new(false),
            stems: Mutex::new(Vec::new()),
            result: Mutex::new(None),
        });
        self.jobs.lock().insert(id.clone(), Arc::clone(&job));

        if self
            .tx
            .send(Work {
                job: Arc::clone(&job),
                format,
            })
            .is_err()
        {
            job.set(State::Failed {
                error: "the separation worker has stopped".to_owned(),
            });
            return Err(ApiError::internal("the separation worker has stopped"));
        }
        Ok((job.view(&self.token), false))
    }

    pub fn get(&self, id: &str) -> Option<JobView> {
        self.jobs.lock().get(id).map(|job| job.view(&self.token))
    }

    pub fn list(&self) -> Vec<JobView> {
        let mut views: Vec<_> = self
            .jobs
            .lock()
            .values()
            .map(|job| job.view(&self.token))
            .collect();
        views.sort_by_key(|v| std::cmp::Reverse(v.created));
        views
    }

    /// Stop a job, or return false when there is no such job.
    ///
    /// A finished job has nothing to cancel, but saying "not found" would make
    /// a DELETE that raced the completion look like a bad id.
    pub fn cancel(&self, id: &str) -> bool {
        let Some(job) = self.jobs.lock().get(id).cloned() else {
            return false;
        };
        job.cancelled.store(true, Ordering::Relaxed);
        true
    }

    /// Forget every job and delete its stems.
    ///
    /// Running jobs are cancelled first and notice between segments. The model
    /// artefacts live outside the jobs root and are not touched.
    pub async fn clear(&self) -> Result<(), ApiError> {
        let cancelled: Vec<_> = {
            let mut jobs = self.jobs.lock();
            jobs.drain().map(|(_, job)| job).collect()
        };
        for job in &cancelled {
            job.cancelled.store(true, Ordering::Relaxed);
        }

        let root = self.root.clone();
        let mut entries = tokio::fs::read_dir(&root)
            .await
            .map_err(|e| ApiError::internal(format!("reading {}: {e}", root.display())))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ApiError::internal(format!("reading {}: {e}", root.display())))?
        {
            let path = entry.path();
            let kind = entry
                .file_type()
                .await
                .map_err(|e| ApiError::internal(format!("stat {}: {e}", path.display())))?;
            let removed = if kind.is_dir() {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_file(&path).await
            };
            match removed {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(ApiError::internal(format!(
                        "removing {}: {err}",
                        path.display()
                    )));
                }
            }
        }

        // The upload landing area is inside the root and was just removed with
        // everything else; put it back for the next job.
        tokio::fs::create_dir_all(root.join("incoming"))
            .await
            .map_err(|e| ApiError::internal(format!("creating {}: {e}", root.display())))?;
        Ok(())
    }

    /// Remove one finished job and its stems.
    ///
    /// A queued or running job is refused rather than racing the worker; stop it
    /// first. The model artefacts are outside the jobs root and are untouched.
    pub async fn remove(&self, id: &str) -> Result<bool, ApiError> {
        let job = self.jobs.lock().get(id).cloned();
        let Some(job) = job else {
            return Ok(false);
        };
        if matches!(
            &*job.state.lock(),
            State::Queued | State::Running { .. }
        ) {
            return Err(ApiError::conflict("stop the job before removing it"));
        }

        job.cancelled.store(true, Ordering::Relaxed);
        self.jobs.lock().remove(id);
        match tokio::fs::remove_dir_all(&job.dir).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(ApiError::internal(format!(
                    "removing {}: {err}",
                    job.dir.display()
                )));
            }
        }
        Ok(true)
    }

    /// A finished job for this source file and output format, if one exists.
    ///
    /// This is what a preflight asks. A running job has no geometry yet and is
    /// not offered; the client uploads and the normal reuse rules apply.
    fn find_source(&self, source_hash: &str, format: StemFormat) -> Option<Arc<Job>> {
        if source_hash.is_empty() {
            return None;
        }
        self.jobs
            .lock()
            .values()
            .find(|job| {
                *job.source_hash.lock() == source_hash
                    && !matches!(
                        &*job.state.lock(),
                        State::Failed { .. } | State::Cancelled
                    )
                    && job
                        .result
                        .lock()
                        .as_ref()
                        .is_some_and(|geometry| geometry.format == format)
            })
            .cloned()
    }

    /// The job a preflight lookup should return.
    pub fn lookup(&self, source_hash: &str, format: StemFormat) -> Option<JobView> {
        self.find_source(source_hash, format)
            .map(|job| job.view(&self.token))
    }

    /// Record the identity a client sent for a job it did not have to create.
    ///
    /// A job can precede the preflight, or be named by an older client whose
    /// hash was computed differently, and the content key still finds it. Until
    /// this runs, every preflight for that track is a 404 even though the job
    /// is listed. Newest value wins: the caller has just supplied the identity
    /// of the file it is holding, and that is the one the next lookup will use.
    ///
    /// A finished job's `detail.json` is the record, so it is rewritten in
    /// place. A queued or running job writes its detail when it finishes, and
    /// that write already takes the updated value.
    fn adopt_source_hash(&self, job: &Arc<Job>, source_hash: Option<&str>) {
        let Some(hash) = source_hash else { return };
        if !job.set_source_hash(hash) {
            return;
        }
        let path = job.dir.join("detail.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // Not finished yet; the completion write will carry it.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::warn!("reading {}: {err}", path.display());
                return;
            }
        };
        let mut detail: Detail = match serde_json::from_slice(&bytes) {
            Ok(detail) => detail,
            Err(err) => {
                tracing::warn!("parsing {}: {err}", path.display());
                return;
            }
        };
        detail.source_hash = hash.to_owned();
        match serde_json::to_vec_pretty(&detail) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(&path, bytes) {
                    tracing::warn!("writing {}: {err}", path.display());
                } else {
                    tracing::debug!("tagged {} with source {hash}", job.id);
                }
            }
            Err(err) => tracing::warn!("serialising {}: {err}", path.display()),
        }
    }

    pub fn stem(&self, id: &str, name: &str) -> Option<Stem> {
        let job = self.jobs.lock().get(id).cloned()?;
        job.stems.lock().iter().find(|s| s.name == name).cloned()
    }
}

#[derive(Clone, Serialize)]
pub struct JobView {
    pub id: String,
    pub created: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    #[serde(flatten)]
    pub state: State,
    /// The desktop API spells the same progress this way, and the CDJ sidecar
    /// reads these fields rather than the nested `progress` object.
    pub stage: String,
    pub completed: u32,
    pub total: u32,
    pub fraction: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    pub stems: Vec<StemView>,
}

#[derive(Clone, Serialize)]
pub struct StemView {
    pub name: String,
    pub url: String,
    pub bytes: u64,
    pub gain: f32,
    pub content_type: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Queued,
    Running { progress: Progress },
    Done { seconds: f64, realtime: f64 },
    Failed { error: String },
    Cancelled,
}

#[derive(Clone)]
pub struct Stem {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub gain: f32,
    pub content_type: &'static str,
}

/// Where a job's audio comes from.
///
/// The app uploads a file; the deck's sidecar streams samples it has already
/// decoded. Both end at the same `Audio`.
enum Source {
    File(PathBuf),
    Pcm {
        bytes: axum::body::Bytes,
        format: PcmFormat,
        sample_rate: u32,
        channels: usize,
    },
    /// A job loaded from disk at startup. It has no audio to run; its stems are
    /// already written.
    None,
}

#[derive(Clone)]
struct Geometry {
    sample_rate: u32,
    channels: usize,
    frames: usize,
    format: StemFormat,
}

struct Job {
    id: String,
    /// Content key: the uploaded audio plus everything that changes the
    /// output. Two submissions with the same key are the same separation.
    key: String,
    /// Client-supplied identity of the source file. Empty when the client did
    /// not provide one; it lets a preflight find a separation before uploading.
    /// Behind a lock because a job that already exists is tagged the first time
    /// a client that knows the identity asks for it.
    source_hash: Mutex<String>,
    created: u64,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    /// The audio to separate. Taken out and dropped as soon as the planar mix
    /// exists: a raw PCM upload is the whole track, and keeping it beside the
    /// mix -- and in the jobs map after the job finished -- doubled the peak
    /// and left every completed job holding its upload until the app was
    /// killed for memory.
    source: Mutex<Source>,
    dir: PathBuf,
    state: Mutex<State>,
    cancelled: AtomicBool,
    stems: Mutex<Vec<Stem>>,
    result: Mutex<Option<Geometry>>,
}

impl Job {
    fn set(&self, state: State) {
        *self.state.lock() = state;
    }

    /// Record the client's identity for the source. True when it changed.
    fn set_source_hash(&self, hash: &str) -> bool {
        let mut held = self.source_hash.lock();
        if *held == hash {
            return false;
        }
        *held = hash.to_owned();
        true
    }

    /// Drop the audio a job was going to separate. For a job that never reaches
    /// the worker -- cancelled, or the model would not load -- this is the only
    /// place the upload is released.
    fn release_source(&self) {
        *self.source.lock() = Source::None;
    }

    fn view(&self, token: &str) -> JobView {
        let state = self.state.lock().clone();
        let (stage, completed, total, fraction) = match &state {
            State::Queued => ("queued".to_owned(), 0, 0, 0.0),
            State::Running { progress } => (
                stage_name(progress.stage).to_owned(),
                progress.completed,
                progress.total,
                progress.fraction,
            ),
            State::Done { .. } => ("done".to_owned(), 0, 0, 1.0),
            State::Failed { .. } => ("failed".to_owned(), 0, 0, 0.0),
            State::Cancelled => ("cancelled".to_owned(), 0, 0, 0.0),
        };
        let geometry = self.result.lock().clone();
        let stems = self
            .stems
            .lock()
            .iter()
            .map(|s| StemView {
                name: s.name.clone(),
                url: format!("/v1/jobs/{}/stems/{}?token={token}", self.id, s.name),
                bytes: s.bytes,
                gain: s.gain,
                content_type: s.content_type,
            })
            .collect();
        JobView {
            id: self.id.clone(),
            created: self.created,
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            state,
            stage,
            completed,
            total,
            fraction,
            sample_rate: geometry.as_ref().map(|g| g.sample_rate),
            channels: geometry.as_ref().map(|g| g.channels),
            frames: geometry.as_ref().map(|g| g.frames),
            format: geometry.as_ref().map(|g| g.format.to_string()),
            stems,
        }
    }
}

/// The name the desktop API uses for a stage, which is also what the CDJ
/// sidecar's `stage_of` matches.
fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Queued => "queued",
        Stage::Analysing => "analysing",
        Stage::Separating => "separating",
        Stage::Reconstructing => "reconstructing",
        Stage::Writing => "writing",
        Stage::Done => "done",
        Stage::Failed => "failed",
        Stage::Cancelled => "cancelled",
    }
}

/// The sink the model reports through, and the flag a DELETE flips.
struct JobSink {
    job: Arc<Job>,
}

impl ProgressSink for JobSink {
    fn update(&self, progress: Progress) {
        self.job.set(State::Running { progress });
    }

    fn cancelled(&self) -> bool {
        self.job.cancelled.load(Ordering::Relaxed)
    }
}

struct Work {
    job: Arc<Job>,
    format: StemFormat,
}

struct WorkerConfig {
    models_dir: PathBuf,
    preset: Preset,
    max_track_secs: f64,
}

fn worker(rx: Receiver<Work>, stop: Arc<AtomicBool>, config: WorkerConfig) {
    let mut separator: Option<Box<dyn Separate>> = None;
    while !stop.load(Ordering::Relaxed) {
        let work = match rx.recv_timeout(IDLE_POLL) {
            Ok(work) => work,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if work.job.cancelled.load(Ordering::Relaxed) {
            work.job.set(State::Cancelled);
            work.job.release_source();
            continue;
        }
        let root = work.job.dir.parent().map(Path::to_path_buf).unwrap_or_default();
        note(&root, &format!("worker start {}", work.job.id));
        if separator.is_none() {
            // MLX's default cache cap is the device's recommended working set,
            // over 7 GB here and more than this app may hold. One separation
            // window fills it in seconds and the process is killed before the
            // between-window release can run. A gigabyte leaves the allocator
            // enough to reuse without letting it become the largest object in
            // the process; the model's live arrays are not affected.
            stemd_core::memory::set_cache_limit(1 << 30);
            match separator::load(&config.models_dir, config.preset) {
                Ok(loaded) => separator = Some(loaded),
                Err(err) => {
                    work.job.set(State::Failed {
                        error: format!("{err:#}"),
                    });
                    work.job.release_source();
                    continue;
                }
            }
        }
        let model: &mut dyn Separate = separator.as_deref_mut().expect("loaded above");
        match separate_one(model, &work.job, work.format, config.max_track_secs) {
            Ok(()) => {}
            Err(err) if Cancelled::caused(&err) => work.job.set(State::Cancelled),
            Err(err) => work.job.set(State::Failed {
                error: format!("{err:#}"),
            }),
        }
        // MLX keeps freed buffers in its allocator cache instead of returning
        // them. The desktop lets an idle timer collect that; an app has no idle
        // timer, and on a phone the cache from one track is enough to make the
        // next one the process's last. Live arrays -- the model -- are not
        // touched, so this only gives back what the separation left behind.
        let freed = stemd_core::memory::release();
        if freed > 0 {
            tracing::debug!(
                "released {:.1} MB of MLX cache after {}",
                freed as f64 / 1e6,
                work.job.id
            );
        }
        note(&root, &format!("cache released freed={freed}"));
    }
}

fn separate_one(
    separator: &mut dyn Separate,
    job: &Arc<Job>,
    format: StemFormat,
    max_track_secs: f64,
) -> Result<()> {
    job.set(State::Running {
        progress: Progress::new(Stage::Analysing),
    });

    // The uploaded audio is only needed to build the mix. Taking it out of the
    // job and letting the match consume it frees a raw PCM upload before the
    // model runs, instead of holding it for the whole separation and then in
    // the jobs map for as long as the job is listed.
    let source = std::mem::replace(&mut *job.source.lock(), Source::None);
    let (audio, source_file) = match source {
        Source::File(path) => {
            let decoded = stemd_audio::decode(&path)
                .with_context(|| format!("decoding {}", path.display()))?;
            let audio = Audio::checked(decoded.data, decoded.sample_rate)
                .with_context(|| format!("decoding {}", path.display()))?;
            (audio, Some(path))
        }
        Source::Pcm {
            bytes,
            format,
            sample_rate,
            channels,
        } => (
            Audio::from_interleaved(&bytes, format, channels, sample_rate)
                .context("decoding the uploaded PCM")?,
            None,
        ),
        Source::None => bail!("a job loaded from disk has no audio to separate"),
    };
    note(
        job.dir.parent().unwrap_or(Path::new(".")),
        &format!("mix built {}", job.id),
    );

    let secs = audio.duration_secs();
    if secs > max_track_secs {
        bail!(
            "the track is {:.1} minutes; this build accepts up to {:.0}. \
             Separation memory grows with length, so a longer file is refused \
             rather than risking the app being killed by the system",
            secs / 60.0,
            max_track_secs / 60.0
        );
    }

    let wanted = separator.info().sample_rate;
    let mix = if audio.sample_rate == wanted {
        audio
    } else {
        tracing::info!(
            "resampling {} Hz to the model's {wanted} Hz",
            audio.sample_rate
        );
        stemd_core::resample::to_rate(&audio, wanted).context("resampling the input")?
    };

    let sink = JobSink {
        job: Arc::clone(job),
    };
    let began = Instant::now();
    let stems = separator.separate(&mix, &sink)?;
    let elapsed = began.elapsed().as_secs_f64();
    note(
        job.dir.parent().unwrap_or(Path::new(".")),
        &format!("separated {}", job.id),
    );

    job.set(State::Running {
        progress: Progress::new(Stage::Writing),
    });
    note(
        job.dir.parent().unwrap_or(Path::new(".")),
        &format!("writing {}", job.id),
    );
    let written = write_stems(job, &mix, &stems, format)?;
    note(
        job.dir.parent().unwrap_or(Path::new(".")),
        &format!("written {}", job.id),
    );
    write_detail(job, &mix, &stems, &written, format, elapsed)?;
    *job.stems.lock() = written;
    *job.result.lock() = Some(Geometry {
        sample_rate: mix.sample_rate,
        channels: mix.channels(),
        frames: mix.frames(),
        format,
    });
    job.set(State::Done {
        seconds: elapsed,
        realtime: if elapsed > 0.0 {
            mix.duration_secs() / elapsed
        } else {
            0.0
        },
    });

    // The source file is no longer needed: the stems are the result, and the
    // app's storage budget is not a place to keep a second copy of every track.
    // A PCM upload never became a file; it was dropped when the mix was built.
    if let Some(path) = source_file {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn write_stems(job: &Job, mix: &Audio, stems: &Stems, format: StemFormat) -> Result<Vec<Stem>> {
    let mut written = Vec::new();
    // The derived part is one more file. Counting them makes "writing" a stage
    // with movement: encoding a full-length track takes long enough that a bar
    // frozen at its stage floor reads as a hang.
    let total = (stems.shipped.len() + 1) as u32;
    for (done, (name, audio)) in stems.shipped.iter().enumerate() {
        written.push(write_stem(&job.dir, name, audio, format)?);
        job.set(State::Running {
            progress: Progress::counted(Stage::Writing, done as u32 + 1, total),
        });
    }
    // A folder of stems has no context to rebuild the third part from, which
    // is why the desktop's drop path writes it too. A file served to another
    // device is the same situation.
    let derived = stems.derived_part(mix);
    written.push(write_stem(&job.dir, "drums", &derived, format)?);
    job.set(State::Running {
        progress: Progress::counted(Stage::Writing, total, total),
    });
    Ok(written)
}

fn write_stem(dir: &Path, name: &str, audio: &Audio, format: StemFormat) -> Result<Stem> {
    let peak = audio
        .peak()
        .with_context(|| format!("the separated {name} is not finite"))?;
    // Same rule as the desktop's transfer gain: a stem that peaks past full
    // scale is scaled to fit, and the gain is recorded so the level can be
    // restored. The float format has no ceiling and stays at unity.
    let gain = if format.has_headroom() || peak <= 1.0 {
        1.0
    } else {
        1.0 / peak
    };
    let bytes = encode_stem(audio, format, gain)?;
    let path = dir.join(format!("{name}.{}", format.extension()));
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(Stem {
        name: name.to_owned(),
        path,
        bytes: bytes.len() as u64,
        gain,
        content_type: format.content_type(),
    })
}

#[derive(Serialize, Deserialize)]
struct Detail {
    /// Content key, absent on entries written before reuse was implemented.
    #[serde(default)]
    key: String,
    /// Source-file identity, absent on entries written before preflight.
    #[serde(default)]
    source_hash: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    format: String,
    sample_rate: u32,
    /// Geometry added with the persistent list. Older entries omit them and the
    /// loader derives both from `duration_secs`.
    #[serde(default)]
    channels: usize,
    #[serde(default)]
    frames: usize,
    duration_secs: f64,
    separation_secs: f64,
    #[serde(default)]
    model_residual_db: f64,
    #[serde(default)]
    stems: Vec<StemDetail>,
}

#[derive(Serialize, Deserialize)]
struct StemDetail {
    name: String,
    file: String,
    bytes: u64,
    gain: f32,
}

fn write_detail(
    job: &Job,
    mix: &Audio,
    stems: &Stems,
    written: &[Stem],
    format: StemFormat,
    elapsed: f64,
) -> Result<()> {
    let detail = Detail {
        key: job.key.clone(),
        source_hash: job.source_hash.lock().clone(),
        title: job.title.clone(),
        artist: job.artist.clone(),
        album: job.album.clone(),
        format: format.to_string(),
        sample_rate: mix.sample_rate,
        channels: mix.channels(),
        frames: mix.frames(),
        duration_secs: mix.duration_secs(),
        separation_secs: elapsed,
        model_residual_db: stems.model_residual_db,
        stems: written
            .iter()
            .map(|s| StemDetail {
                name: s.name.clone(),
                file: s
                    .path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                bytes: s.bytes,
                gain: s.gain,
            })
            .collect(),
    };
    let path = job.dir.join("detail.json");
    let bytes = serde_json::to_vec_pretty(&detail)?;
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stemd-ios-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_stem_over_full_scale_is_scaled_to_fit_and_the_gain_recorded() {
        let dir = temp_dir("gain");
        // The model can over-subtract; a stem at +6 dBFS is what the gain is
        // for. The encoded level must come back inside full scale, and the
        // recorded gain must say what was removed.
        let audio = Audio::new(vec![vec![2.0; 4_410], vec![-2.0; 4_410]], 44_100);
        let stem = write_stem(&dir, "vocals", &audio, StemFormat::Flac).unwrap();
        assert!(stem.gain < 1.0, "gain {}", stem.gain);
        assert!(stem.path.is_file());
        assert!(stem.bytes > 0);
        assert_eq!(stem.content_type, "audio/flac");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stem_inside_full_scale_is_left_at_unity() {
        let dir = temp_dir("unity");
        let audio = Audio::new(vec![vec![0.5; 441], vec![-0.5; 441]], 44_100);
        let stem = write_stem(&dir, "vocals", &audio, StemFormat::Wav).unwrap();
        assert_eq!(stem.gain, 1.0);
        assert_eq!(stem.content_type, "audio/wav");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_jobs_state_is_a_top_level_tag_in_the_json() {
        let view = JobView {
            id: "abc-0".to_owned(),
            created: 0,
            title: Some("Track".to_owned()),
            artist: None,
            album: None,
            state: State::Running {
                progress: Progress::new(Stage::Separating),
            },
            stage: "separating".to_owned(),
            completed: 3,
            total: 10,
            fraction: 0.42,
            sample_rate: None,
            channels: None,
            frames: None,
            format: None,
            stems: Vec::new(),
        };
        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(value["state"], "running");
        assert_eq!(value["id"], "abc-0");
        assert_eq!(value["stage"], "separating");
        assert!(value["progress"]["stage"].is_string());
    }

    #[test]
    fn a_finished_job_is_reloaded_from_its_directory() {
        let root = temp_dir("reload");
        let dir = root.join("abc123-4");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vocals.flac"), b"stem").unwrap();
        let detail = Detail {
            key: "key".to_owned(),
            source_hash: String::new(),
            title: Some("Reloaded".to_owned()),
            artist: Some("Artist".to_owned()),
            album: None,
            format: "flac".to_owned(),
            sample_rate: 44_100,
            channels: 2,
            frames: 441_000,
            duration_secs: 10.0,
            separation_secs: 2.0,
            model_residual_db: 0.0,
            stems: vec![StemDetail {
                name: "vocals".to_owned(),
                file: "vocals.flac".to_owned(),
                bytes: 4,
                gain: 1.0,
            }],
        };
        std::fs::write(
            dir.join("detail.json"),
            serde_json::to_vec(&detail).unwrap(),
        )
        .unwrap();

        // A half-written entry is skipped, not loaded and not deleted.
        let broken = root.join("dead0000-5");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("detail.json"), b"{}").unwrap();

        let (jobs, max) = load_existing(&root);
        assert_eq!(max, 4);
        let job = jobs.get("abc123-4").expect("the complete entry");
        let view = job.view("tok");
        assert_eq!(view.title.as_deref(), Some("Reloaded"));
        assert_eq!(view.created, 4);
        assert_eq!(view.frames, Some(441_000));
        assert_eq!(view.sample_rate, Some(44_100));
        assert_eq!(view.stems.len(), 1);
        assert_eq!(view.stems[0].content_type, "audio/flac");
        match view.state {
            State::Done { seconds, .. } => assert_eq!(seconds, 2.0),
            other => panic!("expected done, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_content_key_covers_what_changes_the_result() {
        let base = content_key(b"audio", StemFormat::Flac, "htdemucs-ios", b"");
        assert_eq!(
            base,
            content_key(b"audio", StemFormat::Flac, "htdemucs-ios", b"")
        );
        assert_ne!(
            base,
            content_key(b"audi0", StemFormat::Flac, "htdemucs-ios", b"")
        );
        assert_ne!(base, content_key(b"audio", StemFormat::Wav, "htdemucs-ios", b""));
        assert_ne!(base, content_key(b"audio", StemFormat::Flac, "other", b""));
        assert_ne!(
            base,
            content_key(b"audio", StemFormat::Flac, "htdemucs-ios", b"f32le")
        );
    }

    fn detail_for(key: &str, title: &str) -> Detail {
        Detail {
            key: key.to_owned(),
            source_hash: String::new(),
            title: Some(title.to_owned()),
            artist: None,
            album: None,
            format: "flac".to_owned(),
            sample_rate: 44_100,
            channels: 2,
            frames: 44_100,
            duration_secs: 1.0,
            separation_secs: 0.5,
            model_residual_db: 0.0,
            stems: vec![StemDetail {
                name: "vocals".to_owned(),
                file: "vocals.flac".to_owned(),
                bytes: 4,
                gain: 1.0,
            }],
        }
    }

    fn write_job(root: &std::path::Path, id: &str, key: &str, title: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vocals.flac"), b"stem").unwrap();
        std::fs::write(
            dir.join("detail.json"),
            serde_json::to_vec(&detail_for(key, title)).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_source_hash_finds_and_reuses_a_finished_job() {
        let root = temp_dir("source-lookup");
        let dir = root.join("abc-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vocals.flac"), b"stem").unwrap();
        let mut detail = detail_for("k", "Title");
        detail.source_hash = "src123".to_owned();
        std::fs::write(
            dir.join("detail.json"),
            serde_json::to_vec(&detail).unwrap(),
        )
        .unwrap();

        let jobs = Jobs::start(Options {
            root: root.clone(),
            models_dir: root.join("models"),
            preset: Preset::Fast,
            max_track_secs: 60.0,
            token: "t".to_owned(),
        })
        .unwrap();

        let hit = jobs.lookup("src123", StemFormat::Flac).expect("lookup hit");
        assert_eq!(hit.id, "abc-1");
        assert!(jobs.lookup("other", StemFormat::Flac).is_none());

        // A POST carrying the same source hash reuses it even with a different
        // body, which is the point of the preflight identity.
        let (view, reused) = jobs
            .submit_pcm(
                axum::body::Bytes::from_static(b"a different body"),
                44_100,
                2,
                PcmFormat::F32le,
                None,
                None,
                None,
                StemFormat::Flac,
                Some("src123"),
            )
            .unwrap();
        assert!(reused);
        assert_eq!(view.id, "abc-1");
        assert_eq!(jobs.count(), 1);

        jobs.stop();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn an_unkeyed_old_entry_is_reused_by_its_body_hash() {
        let root = temp_dir("legacy-reuse");
        let body: &[u8] = b"a-raw-pcm-body";
        let digest = Sha256::digest(body);
        let prefix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        let id = format!("{prefix}-0");
        write_job(&root, &id, "", "Legacy");

        let jobs = Jobs::start(Options {
            root: root.clone(),
            models_dir: root.join("models"),
            preset: Preset::Fast,
            max_track_secs: 60.0,
            token: "t".to_owned(),
        })
        .unwrap();
        assert_eq!(jobs.count(), 1);

        let (view, reused) = jobs
            .submit_pcm(
                axum::body::Bytes::from_static(body),
                44_100,
                2,
                PcmFormat::F32le,
                None,
                None,
                None,
                StemFormat::Flac,
                None,
            )
            .unwrap();
        assert!(reused);
        assert_eq!(view.id, id);
        assert_eq!(jobs.count(), 1);

        jobs.stop();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn reusing_an_old_entry_records_the_source_hash_it_was_told() {
        let root = temp_dir("legacy-tag");
        let body: &[u8] = b"a-raw-pcm-body";
        let digest = Sha256::digest(body);
        let prefix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        let id = format!("{prefix}-0");
        write_job(&root, &id, "", "Legacy");

        let jobs = Jobs::start(Options {
            root: root.clone(),
            models_dir: root.join("models"),
            preset: Preset::Fast,
            max_track_secs: 60.0,
            token: "t".to_owned(),
        })
        .unwrap();
        assert!(jobs.lookup("fresh1", StemFormat::Flac).is_none());

        let (view, reused) = jobs
            .submit_pcm(
                axum::body::Bytes::from_static(body),
                44_100,
                2,
                PcmFormat::F32le,
                None,
                None,
                None,
                StemFormat::Flac,
                Some("fresh1"),
            )
            .unwrap();
        assert!(reused);
        assert_eq!(view.id, id);

        // The point of the whole exercise: the reuse taught the old entry the
        // identity the preflight asks for, so the next load does not upload.
        let hit = jobs
            .lookup("fresh1", StemFormat::Flac)
            .expect("lookup after a tagged reuse");
        assert_eq!(hit.id, id);
        assert_eq!(jobs.count(), 1);

        // And it survives a relaunch, because the detail file is the record.
        let detail: Detail = serde_json::from_slice(
            &std::fs::read(root.join(&id).join("detail.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(detail.source_hash, "fresh1");

        jobs.stop();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn removing_one_job_leaves_the_others() {
        let root = temp_dir("remove");
        write_job(&root, "abc-1", "k1", "One");
        write_job(&root, "def-2", "k2", "Two");

        let jobs = Jobs::start(Options {
            root: root.clone(),
            models_dir: root.join("models"),
            preset: Preset::Fast,
            max_track_secs: 60.0,
            token: "t".to_owned(),
        })
        .unwrap();
        assert_eq!(jobs.count(), 2);

        assert!(jobs.remove("abc-1").await.unwrap());
        assert!(!root.join("abc-1").exists());
        assert!(root.join("def-2").exists());
        assert_eq!(jobs.count(), 1);

        jobs.stop();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn clearing_removes_every_job_directory() {
        let root = temp_dir("clear");
        let job_dir = root.join("abc123-4");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::write(job_dir.join("detail.json"), b"{}").unwrap();

        let jobs = Jobs::start(Options {
            root: root.clone(),
            models_dir: root.join("models"),
            preset: Preset::Fast,
            max_track_secs: 60.0,
            token: "t".to_owned(),
        })
        .unwrap();

        jobs.clear().await.unwrap();
        assert!(!job_dir.exists());
        assert_eq!(jobs.count(), 0);
        assert!(root.join("incoming").is_dir());

        jobs.stop();
        std::fs::remove_dir_all(&root).ok();
    }
}
