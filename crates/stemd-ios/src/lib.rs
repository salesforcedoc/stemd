//! On-device stem separation for iOS.
//!
//! The app links this as a static library and starts it through
//! [`stemd_ios_start`]. The UI talks to the started server over HTTP, which is
//! the same interface the desktop app has, so a client written against
//! `docs/api.md` is not learning a second protocol. What differs is the ends:
//!
//! * the job body is an **encoded audio file**, not raw PCM. The phone already
//!   has the file; decoding it here saves a full-size copy through Swift and
//!   lets `stemd-audio` be the only decoder, which is what keeps encoder delay
//!   consistent with the desktop path.
//! * there is no job cache keyed on uploaded bytes. The app is the library:
//!   each job owns a directory, and the stems are served from it until the app
//!   removes it.
//! * the server is foreground-only. iOS suspends it with the app, and the UI
//!   says so while it is serving.
//!
//! # FFI
//!
//! [`stemd_ios_start`] takes a JSON [`Config`] and returns JSON: `{"port": N}`
//! or `{"error": "..."}`. The returned string is owned by the caller and must
//! be released with [`stemd_ios_free_string`]. [`stemd_ios_stop`] stops the
//! server and the separation worker; it is safe to call when nothing started.

mod api;
mod jobs;
mod models;
mod requests;
mod separator;

use std::ffi::{CStr, CString, c_char};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use stemd_models::Preset;

/// What Swift passes to [`stemd_ios_start`].
///
/// Paths are the app container's, resolved by Swift. Nothing here guesses a
/// directory: iOS has no equivalent of `~/Library/Application Support/stemd`,
/// and the sandbox moves with every install.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where jobs and their stems are kept, under the app container.
    pub data_dir: PathBuf,
    /// Where `<artefact>.safetensors` live. Defaults to `<data_dir>/models`.
    #[serde(default)]
    pub models_dir: Option<PathBuf>,
    /// `fast` (default), `balanced` or `quality`.
    #[serde(default = "default_preset")]
    pub preset: String,
    /// Address to bind. The default is every interface on an ephemeral port,
    /// because the point of serving the stems is that another device fetches
    /// them. `127.0.0.1:0` keeps it to the app.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Longest input accepted, in seconds. Phone memory grows with length; the
    /// desktop's 90-minute cap is not a phone budget.
    #[serde(default = "default_max_track_secs")]
    pub max_track_secs: f64,
    /// Shared secret for the HTTP surface.
    ///
    /// Generated when omitted, and returned by [`start`]. The server can be
    /// bound to the local network so another device can fetch stems, which is
    /// also a network this app does not control; an unauthenticated endpoint
    /// there would let anyone on it read the user's audio.
    #[serde(default)]
    pub token: Option<String>,
    /// When false, requests without a token are accepted.
    ///
    /// The CDJ/emulator client has no way to send one, so compatibility mode
    /// turns the requirement off. It is a setting rather than a default
    /// because the same server can be reached from whatever network the phone
    /// is on.
    #[serde(default = "default_require_token")]
    pub require_token: bool,
}

fn default_require_token() -> bool {
    true
}

fn default_preset() -> String {
    "fast".to_owned()
}

fn default_bind() -> String {
    "0.0.0.0:0".to_owned()
}

fn default_max_track_secs() -> f64 {
    15.0 * 60.0
}

impl Config {
    fn preset(&self) -> Result<Preset> {
        match self.preset.as_str() {
            "fast" | "Fast" => Ok(Preset::Fast),
            "balanced" | "Balanced" => Ok(Preset::Balanced),
            "quality" | "Quality" => Ok(Preset::Quality),
            other => anyhow::bail!("unknown preset {other:?}; expected fast, balanced or quality"),
        }
    }

    fn models_dir(&self) -> PathBuf {
        self.models_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("models"))
    }
}

/// What a successful start returns, and what the FFI serialises.
#[derive(Debug, Serialize)]
pub struct Started {
    pub port: u16,
    /// The token every request must carry. Also embedded in stem URLs, so a
    /// player can fetch one without knowing the header convention.
    pub token: String,
    /// False when compatibility mode is on and the token is optional.
    pub token_required: bool,
}

/// A started server, kept alive by the static slot below.
struct Running {
    port: u16,
    shutdown: tokio::sync::oneshot::Sender<()>,
    runtime: tokio::runtime::Runtime,
    jobs: std::sync::Arc<jobs::Jobs>,
    /// Flipped by the debug screen's compatibility switch.
    open: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

static RUNNING: OnceLock<Mutex<Option<Running>>> = OnceLock::new();

fn running() -> &'static Mutex<Option<Running>> {
    RUNNING.get_or_init(|| Mutex::new(None))
}

/// Start the server and the separation worker, returning the bound port.
///
/// Blocking until the listener is bound, so a caller that wants to print or
/// show the URL does not have to poll. Model loading does not happen here: the
/// first job pays that cost on the worker thread, which keeps start fast
/// enough to be worth calling from a launch path.
pub fn start(config: Config) -> Result<Started> {
    stemd_ios_init_logging();

    {
        let slot = running().lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            anyhow::bail!("stemd-ios is already running; call stemd_ios_stop first");
        }
    }

    let preset = config.preset()?;
    let models_dir = config.models_dir();
    let root = config.data_dir.join("jobs");
    let token = match config.token {
        Some(token) if !token.is_empty() => token,
        _ => generate_token()?,
    };

    let models = models::Models::new(models_dir.clone(), preset);
    let jobs = jobs::Jobs::start(jobs::Options {
        root,
        models_dir,
        preset,
        max_track_secs: config.max_track_secs,
        token: token.clone(),
    })?;
    let open = Arc::new(std::sync::atomic::AtomicBool::new(!config.require_token));
    let requests = Arc::new(requests::RequestLog::new());

    let bind: SocketAddr = config
        .bind
        .parse()
        .with_context(|| format!("bind address {:?}", config.bind))?;

    let app = api::router(Arc::clone(&jobs), models, requests, Arc::clone(&open));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("stemd-ios")
        .build()
        .context("building the tokio runtime")?;
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(bind))
        .with_context(|| format!("binding {bind}"))?;
    let port = listener
        .local_addr()
        .context("reading the bound address")?
        .port();

    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
        // Connect info is what lets the debug screen separate the app's
        // loopback calls from a deck on the LAN.
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        });
        if let Err(err) = served.await {
            tracing::error!("stemd-ios server stopped: {err}");
        }
    });

    let token_required = !open.load(std::sync::atomic::Ordering::Relaxed);
    let mut slot = running().lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(Running {
        port,
        shutdown,
        runtime,
        jobs,
        open,
    });

    tracing::info!("stemd-ios listening on {bind} (port {port})");
    Ok(Started {
        port,
        token,
        token_required,
    })
}

/// A URL-safe random secret. Hex of 16 bytes: 128 bits, and no escaping needed
/// in a query string.
fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("generating a token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Stop the server and the separation worker.
pub fn stop() {
    let taken = running().lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(running) = taken {
        let _ = running.shutdown.send(());
        running.jobs.stop();
        running
            .runtime
            .shutdown_timeout(std::time::Duration::from_secs(2));
        tracing::info!("stemd-ios stopped on port {}", running.port);
    }
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

/// Start from a JSON [`Config`], returning a JSON result string.
///
/// The caller owns the returned pointer and must pass it to
/// [`stemd_ios_free_string`]. Never panics across the boundary: a panic is
/// turned into an error object and the runtime is left stopped.
#[unsafe(no_mangle)]
pub extern "C" fn stemd_ios_start(config_json: *const c_char) -> *mut c_char {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if config_json.is_null() {
            return Err(anyhow::anyhow!("config_json is null"));
        }
        // SAFETY: the caller promises a NUL-terminated string it owns for the
        // duration of the call.
        let json = unsafe { CStr::from_ptr(config_json) }
            .to_str()
            .context("config_json is not UTF-8")?;
        let config: Config = serde_json::from_str(json).context("parsing the config")?;
        start(config)
    }));

    let payload = match result {
        Ok(Ok(started)) => serde_json::to_string(&started)
            .unwrap_or_else(|_| "{\"error\":\"could not serialise the start result\"}".to_owned()),
        Ok(Err(err)) => serde_json::json!({ "error": format!("{err:#}") }).to_string(),
        Err(_) => serde_json::json!({ "error": "stemd-ios panicked while starting" }).to_string(),
    };
    // The payload is generated here and contains no interior NUL: serde_json
    // escapes control characters. The fallback is still a valid JSON object so
    // a caller that always parses the result never sees an empty string.
    CString::new(payload)
        .unwrap_or_else(|_| {
            CString::new("{\"error\":\"internal NUL in the result\"}").expect("literal has no NUL")
        })
        .into_raw()
}

/// Stop the server. Safe to call when nothing is running.
#[unsafe(no_mangle)]
pub extern "C" fn stemd_ios_stop() {
    let _ = std::panic::catch_unwind(stop);
}

/// Require (or stop requiring) the share token for requests.
///
/// The compatibility switch for clients that cannot send one, such as the
/// CDJ/emulator sidecar. Takes effect immediately; no restart, no lost jobs.
#[unsafe(no_mangle)]
pub extern "C" fn stemd_ios_set_token_required(required: i32) {
    if let Some(running) = running().lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        running
            .open
            .store(required == 0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Release a string returned by [`stemd_ios_start`].
///
/// # Safety
///
/// `ptr` must come from [`stemd_ios_start`] and must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stemd_ios_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        // SAFETY: the pointer came from `CString::into_raw` in this module, and
        // the caller promised not to use it again.
        drop(unsafe { CString::from_raw(ptr) });
    }
}

/// Initialise logging once. Called from `start`; exposed so a caller can do it
/// earlier if it wants launch-time messages in the console.
#[unsafe(no_mangle)]
pub extern "C" fn stemd_ios_init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .try_init();
}

/// Describes what the app is running, for a settings screen or a bug report.
#[derive(Debug, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub target: &'static str,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        target: std::env::consts::ARCH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_presets_are_named_the_way_the_app_sends_them() {
        for (name, preset) in [
            ("fast", Preset::Fast),
            ("balanced", Preset::Balanced),
            ("quality", Preset::Quality),
        ] {
            let config: Config = serde_json::from_value(serde_json::json!({
                "data_dir": "/tmp/stemd-ios-test",
                "preset": name,
            }))
            .unwrap();
            assert_eq!(config.preset().unwrap(), preset);
        }
    }

    #[test]
    fn an_unknown_preset_is_refused_rather_than_defaulted() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "data_dir": "/tmp/stemd-ios-test",
            "preset": "turbo",
        }))
        .unwrap();
        let err = config.preset().unwrap_err().to_string();
        assert!(err.contains("turbo"), "{err}");
    }

    #[test]
    fn models_default_to_a_directory_inside_the_app_container() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "data_dir": "/tmp/app",
        }))
        .unwrap();
        assert_eq!(
            config.models_dir(),
            std::path::PathBuf::from("/tmp/app/models")
        );
        assert_eq!(config.preset().unwrap(), Preset::Fast);
        assert_eq!(config.max_track_secs, 15.0 * 60.0);
    }
}
