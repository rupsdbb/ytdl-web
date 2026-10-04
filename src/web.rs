//! HTTP API and embedded web UI.

use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_stream::wrappers::WatchStream;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::downloads::{Downloads, JobInfo, StartError};
use crate::formats::{self, Choice};
use crate::ytdlp::{YtDlp, friendly_error, truncate};

/// Concurrent info lookups (each one is a yt-dlp process).
const INFO_SLOTS: usize = 2;

#[derive(Clone)]
pub struct AppState {
    ytdlp: Arc<YtDlp>,
    downloads: Arc<Downloads>,
    info_slots: Arc<Semaphore>,
    updating: Arc<Mutex<()>>,
    version: Arc<RwLock<Option<String>>>,
    sites: Arc<RwLock<Option<String>>>,
    shutdown: CancellationToken,
}

impl AppState {
    pub fn new(ytdlp: Arc<YtDlp>, downloads: Arc<Downloads>, shutdown: CancellationToken) -> Self {
        AppState {
            ytdlp,
            downloads,
            info_slots: Arc::new(Semaphore::new(INFO_SLOTS)),
            updating: Arc::new(Mutex::new(())),
            version: Arc::default(),
            sites: Arc::default(),
            shutdown,
        }
    }
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
    fn conflict(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::CONFLICT, msg.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, truncate(&format!("{e:#}"), 600))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|h| asset(h, "text/html; charset=utf-8", include_bytes!("../static/index.html"))))
        .route("/app.js", get(|h| asset(h, "text/javascript; charset=utf-8", include_bytes!("../static/app.js"))))
        .route("/style.css", get(|h| asset(h, "text/css; charset=utf-8", include_bytes!("../static/style.css"))))
        .route("/favicon.svg", get(|h| asset(h, "image/svg+xml", include_bytes!("../static/favicon.svg"))))
        .route(
            "/manifest.webmanifest",
            get(|h| asset(h, "application/manifest+json", include_bytes!("../static/manifest.webmanifest"))),
        )
        .route("/icon-192.png", get(|h| asset(h, "image/png", include_bytes!("../static/icon-192.png"))))
        .route("/icon-512.png", get(|h| asset(h, "image/png", include_bytes!("../static/icon-512.png"))))
        .route("/api/info", post(info))
        .route("/api/download", post(start))
        .route("/api/cancel", post(cancel))
        .route("/api/events", get(events))
        .route("/api/file/{job}", get(file))
        .route("/api/version", get(version))
        .route("/api/update", post(update))
        .route("/api/sites", get(sites))
        .layer(middleware::from_fn(csrf_guard))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Embedded static file, revalidated by content hash so upgrades are picked up.
async fn asset(headers: HeaderMap, content_type: &'static str, body: &'static [u8]) -> Response {
    let mut hasher = std::hash::DefaultHasher::new();
    body.hash(&mut hasher);
    let etag = format!("\"{:016x}\"", hasher.finish());
    if headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == etag.as_bytes()) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

async fn security_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    // Thumbnails come from the video site's CDN.
    let csp = "default-src 'self'; img-src 'self' https: data:; style-src 'self'; script-src 'self'; \
               connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(csp));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if is_api && !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

/// There is no login, so stop other web pages from making the browser
/// start downloads or updates here: they can't add a custom header without
/// a CORS preflight, which this server never approves.
async fn csrf_guard(req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && req.headers().get("x-ytdl").is_none_or(|v| v != "1") {
        return ApiError(StatusCode::FORBIDDEN, "missing X-Ytdl header".into()).into_response();
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct UrlBody {
    url: String,
}

#[derive(Serialize)]
struct InfoResponse {
    title: String,
    uploader: Option<String>,
    duration: Option<u64>,
    thumbnail: Option<String>,
    formats: Vec<Choice>,
}

fn check_url(url: &str) -> ApiResult<&str> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(ApiError::bad_request("Enter a link starting with http:// or https://"));
    }
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ApiError::bad_request("That doesn't look like a valid link"));
    }
    Ok(url)
}

async fn info(State(st): State<AppState>, Json(body): Json<UrlBody>) -> ApiResult<Json<InfoResponse>> {
    let url = check_url(&body.url)?;
    let Ok(_slot) = st.info_slots.try_acquire() else {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Already looking up other links, try again shortly".into(),
        ));
    };
    let raw = st.ytdlp.info(url).await.map_err(|e| {
        tracing::info!("lookup of {url} failed: {e:#}");
        ApiError::bad_request(friendly_error(&format!("{e:#}")))
    })?;
    if matches!(raw.kind.as_deref(), Some("playlist" | "multi_video")) {
        return Err(ApiError::bad_request("That is a playlist or channel. Paste the link of a single video."));
    }
    if raw.formats.is_empty() {
        return Err(ApiError::bad_request("No downloadable video found at that link"));
    }
    Ok(Json(InfoResponse {
        title: raw.title.unwrap_or_else(|| "Untitled".into()),
        uploader: raw.uploader.or(raw.channel),
        duration: raw.duration.filter(|d| d.is_finite() && *d >= 0.0).map(|d| d.round() as u64),
        thumbnail: raw.thumbnail.filter(|t| t.starts_with("https://")),
        formats: formats::choices(&raw.formats, raw.duration),
    }))
}

#[derive(Deserialize)]
struct StartBody {
    url: String,
    format: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    label: String,
}

async fn start(State(st): State<AppState>, Json(body): Json<StartBody>) -> ApiResult<Json<serde_json::Value>> {
    let url = check_url(&body.url)?.to_string();
    if !formats::valid_selector(&body.format) {
        return Err(ApiError::bad_request("Invalid format"));
    }
    // Held until the download has claimed its slot, so an update can't slip
    // in between this check and the claim (update checks for a running download).
    let Ok(_no_update) = st.updating.try_lock() else {
        return Err(ApiError::conflict("yt-dlp is being updated, try again in a moment"));
    };
    let about = JobInfo {
        title: truncate(body.title.trim(), 300),
        thumbnail: body.thumbnail.filter(|t| t.starts_with("https://") && t.len() <= 2048),
        label: truncate(body.label.trim(), 100),
    };
    match st.downloads.start(url, body.format, about) {
        Ok(job) => Ok(Json(json!({ "job": job }))),
        Err(StartError::Busy) => Err(ApiError::conflict("Another download is already in progress")),
        Err(StartError::Io(e)) => Err(anyhow::Error::from(e).context("creating the download directory").into()),
    }
}

async fn cancel(State(st): State<AppState>) -> ApiResult<StatusCode> {
    if st.downloads.cancel() { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::conflict("No download in progress")) }
}

async fn events(State(st): State<AppState>) -> impl IntoResponse {
    let stream = WatchStream::new(st.downloads.subscribe())
        .map(|status| Event::default().json_data(&status))
        .take_until(st.shutdown.cancelled_owned());
    // nginx would otherwise hold events back in its buffer.
    ([("x-accel-buffering", "no")], Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Streams a finished download. HEAD only checks that it is still there.
/// The file is deleted once a GET has read all of it, so an interrupted
/// transfer can be retried.
async fn file(State(st): State<AppState>, method: Method, Path(job): Path<u64>) -> ApiResult<Response> {
    let gone = || ApiError(StatusCode::NOT_FOUND, "This download is no longer available".into());
    let ready = st.downloads.ready_file(job).ok_or_else(gone)?;
    let file = tokio::fs::File::open(&ready.path).await.map_err(|_| gone())?;
    let len = file.metadata().await.map_err(anyhow::Error::from)?.len();
    let headers = [
        (header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream")),
        (header::CONTENT_DISPOSITION, content_disposition(&ready.name)),
        (header::CONTENT_LENGTH, HeaderValue::from(len)),
    ];
    if method == Method::HEAD {
        return Ok((headers, Body::empty()).into_response());
    }
    let downloads = st.downloads.clone();
    let body = OnComplete {
        inner: ReaderStream::with_capacity(file, 256 * 1024),
        remaining: len,
        done: Some(Box::new(move || downloads.delivered(job))),
    };
    Ok((headers, Body::from_stream(body)).into_response())
}

/// Calls `done` once all `remaining` bytes have been read. (With a
/// Content-Length, hyper stops polling after the last byte instead of
/// waiting for the end of the stream.)
struct OnComplete<S> {
    inner: S,
    remaining: u64,
    done: Option<Box<dyn FnOnce() + Send>>,
}

impl<S: Stream<Item = std::io::Result<Bytes>> + Unpin> Stream for OnComplete<S> {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = std::task::ready!(self.inner.poll_next_unpin(cx));
        if let Some(Ok(chunk)) = &next {
            self.remaining = self.remaining.saturating_sub(chunk.len() as u64);
        }
        if (self.remaining == 0 || next.is_none())
            && let Some(done) = self.done.take()
        {
            done();
        }
        Poll::Ready(next)
    }
}

/// `attachment` with an ASCII fallback name plus the exact UTF-8 name (RFC 6266).
fn content_disposition(name: &str) -> HeaderValue {
    let fallback: String =
        name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut encoded = String::with_capacity(name.len() * 3);
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    HeaderValue::from_str(&format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"))
        .unwrap_or(HeaderValue::from_static("attachment"))
}

async fn version(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    if let Some(v) = st.version.read().await.clone() {
        return Ok(Json(json!({ "version": v })));
    }
    let v = st.ytdlp.version().await?;
    *st.version.write().await = Some(v.clone());
    Ok(Json(json!({ "version": v })))
}

async fn update(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let Ok(_guard) = st.updating.try_lock() else {
        return Err(ApiError::conflict("An update check is already running"));
    };
    if st.downloads.is_busy() {
        return Err(ApiError::conflict("Can't update while a download is in progress"));
    }
    let before = st.ytdlp.version().await?;
    st.ytdlp.update().await?;
    let after = st.ytdlp.version().await?;
    *st.version.write().await = Some(after.clone());
    let updated = before != after;
    if updated {
        // The list of supported sites changes with the version.
        *st.sites.write().await = None;
        tracing::info!("yt-dlp updated from {before} to {after}");
    }
    let message = if updated { format!("Updated yt-dlp to {after}") } else { format!("yt-dlp {after} is up to date") };
    Ok(Json(json!({ "updated": updated, "version": after, "message": message })))
}

async fn sites(State(st): State<AppState>) -> ApiResult<Response> {
    if let Some(s) = st.sites.read().await.clone() {
        return Ok(text(s));
    }
    let s = st.ytdlp.extractors().await?;
    *st.sites.write().await = Some(s.clone());
    Ok(text(s))
}

fn text(s: String) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], s).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_keeps_unicode_names() {
        let v = content_disposition("Café \"live\".mp4");
        assert_eq!(
            v.to_str().unwrap(),
            "attachment; filename=\"Caf_ _live_.mp4\"; filename*=UTF-8''Caf%C3%A9%20%22live%22.mp4"
        );
    }

    #[test]
    fn urls() {
        assert!(check_url(" https://youtu.be/abc ").is_ok());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("--exec=rm").is_err());
        assert!(check_url("https://a b").is_err());
    }
}
