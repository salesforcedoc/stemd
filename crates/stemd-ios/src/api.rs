//! The HTTP surface the app talks to.
//!
//! Shape follows the desktop API where it can, so `docs/api.md` is still the
//! reference for clients. Two deliberate differences:
//!
//! * `POST /v1/jobs` takes an encoded audio file. The desktop takes raw PCM
//!   because its clients already decoded; here the file is the input, and
//!   decoding it in Rust is what keeps one decoder in the path.
//! * there is no cache lookup to go with it. A job's directory is its cache,
//!   and the app decides when to delete it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Path as UrlPath, Query, Request, State as AxumState},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
use stemd_core::{PcmFormat, StemFormat};
use tokio_util::io::ReaderStream;

use crate::jobs::{JobView, Jobs};
use crate::models::Models;
use crate::requests::RequestLog;

/// Largest upload accepted. A 10-minute 24-bit/96 kHz WAV is about 350 MB;
/// beyond that the phone is out of memory before the model starts.
const MAX_UPLOAD: usize = 512 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    jobs: Arc<Jobs>,
    models: Arc<Models>,
    requests: Arc<RequestLog>,
    /// True when the token requirement is off (CDJ compatibility).
    open: Arc<AtomicBool>,
}

pub fn router(
    jobs: Arc<Jobs>,
    models: Arc<Models>,
    requests: Arc<RequestLog>,
    open: Arc<AtomicBool>,
) -> Router {
    let state = AppState {
        jobs,
        models,
        requests,
        open,
    };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/jobs", get(list).post(create).delete(clear_all))
        .route("/v1/jobs/lookup", get(lookup))
        .route("/v1/jobs/{id}", get(one).delete(cancel))
        .route(
            "/v1/jobs/{id}/cache",
            axum::routing::delete(remove_cached),
        )
        .route("/v1/jobs/{id}/stems/{name}", get(stem))
        .route("/v1/models/download", axum::routing::post(download_models))
        .route("/v1/server/requests", get(server_requests))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ))
        // Outermost: a request that fails the token check is still a request
        // the debug screen should show.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            record_request,
        ))
        .with_state(state)
}

/// Record every request, including the ones rejected by the token check.
async fn record_request(
    AxumState(state): AxumState<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_default();
    // Fetching the log must not add a row about fetching the log.
    if path.starts_with("/v1/server/requests") {
        return next.run(request).await;
    }
    let method = request.method().as_str().to_owned();
    let began = Instant::now();
    let response = next.run(request).await;
    state.requests.record(
        &method,
        &path,
        response.status().as_u16(),
        began.elapsed(),
        remote,
    );
    response
}

/// The server can be reachable from the local network, which is not a network
/// this app controls. Everything except the health probe needs the token, as a
/// bearer header or a `token` query parameter. The query spelling exists so a
/// stem URL can be handed to a player or a browser unchanged.
async fn require_token(
    AxumState(state): AxumState<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.open.load(Ordering::Relaxed) || request.uri().path() == "/v1/health" {
        return Ok(next.run(request).await);
    }
    let token = state.jobs.token();
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == token);
    let query = request.uri().query().is_some_and(|query| {
        let expected = format!("token={token}");
        query.split('&').any(|pair| pair == expected.as_str())
    });
    if bearer || query {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::unauthorized())
    }
}

async fn health(AxumState(state): AxumState<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        // The desktop API's fields, so a deck-side client can decide whether
        // this server can feed it before uploading anything.
        "version": env!("CARGO_PKG_VERSION"),
        "backend": "mlx",
        "model": state.jobs.model_name(),
        "device": "mlx",
        "sample_rate": 44_100,
        "channels": 2,
        "stems": stemd_core::SHIPPED,
        "preset": state.jobs.preset_label(),
        "model_id": state.jobs.model_id(),

        "models": state.models.status(),
        "max_track_secs": state.jobs.max_track_secs(),
        "jobs": state.jobs.count(),
        "token_required": !state.open.load(Ordering::Relaxed),
    }))
}

/// The request log, for the app's debug screen.
async fn server_requests(AxumState(state): AxumState<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "requests": state.requests.recent() }))
}

/// Fetch the weights for the configured preset in the background.
///
/// `202` means started; `409` means one is already running. Progress is in
/// `/v1/health`, under `models`.
async fn download_models(AxumState(state): AxumState<AppState>) -> Result<StatusCode, ApiError> {
    if state.models.start_download() {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::conflict("a model download is already running"))
    }
}

#[derive(Debug, Deserialize)]
struct CreateParams {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    /// The source file name, used only as the decoder's extension hint.
    filename: Option<String>,
    /// Identity of the source file, from the client. Lets the server reuse a
    /// finished separation before the body is decoded.
    source_hash: Option<String>,
    /// The desktop API spells the body's encoding here (`f32le`, `s16le`);
    /// otherwise this app's original API spelled the output container here.
    format: Option<String>,
    /// The desktop spelling for the output container.
    output_format: Option<String>,
    /// Raw-upload geometry, desktop spelling.
    sample_rate: Option<u32>,
    channels: Option<usize>,
    /// Accepted for compatibility. The model runs at 44.1 kHz and the stems
    /// ship at that rate; the deck resamples what it receives.
    output_sample_rate: Option<u32>,
    dsp_mode: Option<u32>,
}

fn output_format(primary: Option<&str>, fallback: Option<&str>) -> Result<StemFormat, ApiError> {
    match primary.or(fallback) {
        None => Ok(StemFormat::Flac),
        Some("flac") => Ok(StemFormat::Flac),
        Some("wav") => Ok(StemFormat::Wav),
        Some("pcm16" | "s16le") => Ok(StemFormat::Pcm16),
        Some("pcm32" | "f32le") => Ok(StemFormat::Pcm32),
        Some("mp3") => Err(ApiError::bad_request(
            "MP3 output is not built for iOS; ask for flac or wav",
        )),
        Some(other) => Err(ApiError::bad_request(format!(
            "unknown output format {other:?}; expected flac, wav, pcm16 or pcm32"
        ))),
    }
}

/// Accept a track and queue it.
///
/// Two spellings arrive here:
///
/// * this app's: the body is an encoded file (`filename` gives the decoder its
///   extension hint); `format` names the output container.
/// * the desktop API's: `format=f32le|s16le` with `sample_rate`/`channels`, and
///   the body is raw interleaved samples. This is what the CDJ sidecar sends.
async fn create(
    AxumState(state): AxumState<AppState>,
    Query(params): Query<CreateParams>,
    body: Bytes,
) -> Result<(StatusCode, Json<JobView>), ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request("the request body is empty"));
    }

    let input_pcm = match params.format.as_deref() {
        Some("s16le" | "pcm16") => Some(PcmFormat::S16le),
        Some("f32le" | "pcm32") => Some(PcmFormat::F32le),
        _ => None,
    };

    if let Some(pcm) = input_pcm {
        let sample_rate = params.sample_rate.unwrap_or(44_100);
        let channels = params.channels.unwrap_or(2);
        if sample_rate == 0 {
            return Err(ApiError::bad_request("sample_rate must be positive"));
        }
        if channels != 2 {
            return Err(ApiError::bad_request(
                "this build separates stereo; send channels=2",
            ));
        }
        if params.output_sample_rate.is_some() || params.dsp_mode.is_some() {
            tracing::debug!(
                "compatibility upload asked for output_sample_rate/dsp_mode; \
                 serving the model's own rate"
            );
        }
        let (job, reused) = state.jobs.submit_pcm(
            body,
            sample_rate,
            channels,
            pcm,
            params.title,
            params.artist,
            params.album,
            output_format(params.output_format.as_deref(), None)?,
            params.source_hash.as_deref(),
        )?;
        return Ok((
            if reused {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            },
            Json(job),
        ));
    }

    let (job, reused) = state
        .jobs
        .submit_file(
            body,
            params.title,
            params.artist,
            params.album,
            output_format(params.output_format.as_deref(), params.format.as_deref())?,
            params.filename.as_deref(),
            params.source_hash.as_deref(),
        )
        .await?;
    Ok((
        if reused {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        Json(job),
    ))
}

async fn list(AxumState(state): AxumState<AppState>) -> Json<Vec<JobView>> {
    Json(state.jobs.list())
}

/// Delete every separation and its stems. Model files are not part of this.
async fn clear_all(AxumState(state): AxumState<AppState>) -> Result<StatusCode, ApiError> {
    state.jobs.clear().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct LookupParams {
    /// Client-computed identity of the source file.
    source_hash: String,
    format: Option<String>,
}

/// Ask whether a source file already has a separation, before uploading it.
async fn lookup(
    AxumState(state): AxumState<AppState>,
    Query(params): Query<LookupParams>,
) -> Result<Json<JobView>, ApiError> {
    let format = output_format(params.format.as_deref(), None)?;
    state
        .jobs
        .lookup(params.source_hash.trim(), format)
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no cached separation for that source"))
}

async fn one(
    AxumState(state): AxumState<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<JobView>, ApiError> {
    state
        .jobs
        .get(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no such job"))
}

/// Stop a job. The worker notices between segments, so a running separation is
/// not killed mid-model.
async fn cancel(
    AxumState(state): AxumState<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    if state.jobs.cancel(&id) {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::not_found("no such job"))
    }
}

/// Remove one finished job and its stems from the cache.
async fn remove_cached(
    AxumState(state): AxumState<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    if state.jobs.remove(&id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such job"))
    }
}

async fn stem(
    AxumState(state): AxumState<AppState>,
    UrlPath((id, name)): UrlPath<(String, String)>,
) -> Result<Response, ApiError> {
    let file = state
        .jobs
        .stem(&id, &name)
        .ok_or_else(|| ApiError::not_found("no such stem"))?;
    let handle = tokio::fs::File::open(&file.path)
        .await
        .map_err(|e| ApiError::internal(format!("opening {}: {e}", file.path.display())))?;
    let len = handle
        .metadata()
        .await
        .map_err(|e| ApiError::internal(format!("stat {}: {e}", file.path.display())))?
        .len();
    Response::builder()
        .header(header::CONTENT_TYPE, file.content_type)
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from_stream(ReaderStream::new(handle)))
        .map_err(|e| ApiError::internal(format!("building the response: {e}")))
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    pub(crate) fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or wrong token; see the start result".to_owned(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(format!("{err:#}"))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
