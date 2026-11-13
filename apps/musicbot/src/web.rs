// SPDX-License-Identifier: EUPL-1.2
//! The web page: a Svelte app (apps/musicbot/web) served below `/u/<token>/`,
//! a JSON API for it, and a stream of state changes (Server-Sent Events) so
//! that every viewer sees what anybody changes. All the app's addresses are
//! relative to the link, so it also works behind a proxy under a path. The
//! contract is described in docs/musicbot.md ("Web API").
use crate::{
    Bot,
    player::{BrowserSubscription, Ducking, MAX_DUCK_HOLD},
    subsonic::{
        self, Album, AlbumList, Artist, Library, LibraryIdentity, Playlist, SongDetails,
        SonicMatch, Track,
    },
    tokens::Owner,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{
        Html, IntoResponse, Redirect, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{Semaphore, SemaphorePermit, mpsc};
use uuid::Uuid;

/// Searches longer than this are cut.
const MAX_QUERY: usize = 200;
/// How long a request waits for a library.
const LIBRARY_TIMEOUT: Duration = subsonic::API_TIMEOUT;
/// Count actual blocking calls, not callers still waiting for their replies.
const MAX_LIBRARY_CALLS: usize = 8;
static LIBRARY_CALLS: Semaphore = Semaphore::const_new(MAX_LIBRARY_CALLS);
/// A body owns its slot until the SSE stream ends or is dropped. No waiters.
const MAX_SSE_WATCHERS: usize = 32;
static SSE_WATCHERS: Semaphore = Semaphore::const_new(MAX_SSE_WATCHERS);
/// Includes fetching and the actual isolated worker exit, not HTTP lifetime.
const MAX_COVER_JOBS: usize = 2;
static COVER_JOBS: Semaphore = Semaphore::const_new(MAX_COVER_JOBS);
/// Finished cover responses shared by every viewer, so the library and the
/// worker handle each cover and size at most once a day.
const COVER_CACHE_BYTES: usize = 64 * 1024 * 1024;
const COVER_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
static COVERS: std::sync::LazyLock<std::sync::Mutex<CoverCache>> = std::sync::LazyLock::new(|| {
    std::sync::Mutex::new(CoverCache::new(COVER_CACHE_BYTES, COVER_CACHE_TTL))
});
/// Live bodies have their own admission, independent of library metadata calls.
const MAX_LISTEN_STREAMS: usize = 16;
static LISTEN_STREAMS: Semaphore = Semaphore::const_new(MAX_LISTEN_STREAMS);
/// A listening stream with nothing to send (a paused or waiting queue) sends
/// this one-byte record at least this often, so proxies with read timeouts
/// (nginx defaults to 60 s) keep it open.
const LISTEN_KEEPALIVE: Duration = Duration::from_secs(15);
const KEEPALIVE_RECORD: &[u8] = &[2];
/// Albums per page of an album list.
const PAGE_SIZE: u32 = 48;
/// Changes that come in a burst are sent as one state.
const COALESCE: Duration = Duration::from_millis(50);
const INVALID_LINK: &str =
    "This link is not valid (any more). Send !web to the music bot in Mumble for a new one.";
/// Remembers the browser's own link, so token-free shared URLs can open it.
const LINK_COOKIE: &str = "musicbot_link";
const LINK_COOKIE_MAX_AGE: u64 = 90 * 24 * 60 * 60;

pub fn router(bot: Arc<Bot>) -> Router {
    let links = Router::new()
        .route("/", get(root))
        .route("/u/{token}", get(slash))
        .route("/u/{token}/", get(remember))
        .route("/u/{token}/assets/{file}", get(asset))
        .route("/u/{token}/licenses", get(licenses))
        .route("/u/{token}/art/{library}/{id}", get(art))
        .route("/u/{token}/api/state", get(state))
        .route("/u/{token}/api/history", get(history))
        .route("/u/{token}/api/events", get(events))
        .route("/u/{token}/api/listen", get(listen))
        .route("/u/{token}/api/command", post(command))
        .route("/u/{token}/api/rating", get(rating).post(set_rating))
        .route("/u/{token}/api/albums", get(albums))
        .route("/u/{token}/api/album", get(album))
        .route("/u/{token}/api/artists", get(artists))
        .route("/u/{token}/api/artist", get(artist))
        .route("/u/{token}/api/playlists", get(playlists))
        .route("/u/{token}/api/playlist", get(playlist))
        .route("/u/{token}/api/search", get(search))
        .route("/u/{token}/api/song", get(song))
        .route("/u/{token}/api/similar", get(similar))
        .route("/u/{token}/api/path", get(sonic_path))
        .layer(axum::middleware::map_response(headers))
        .with_state(bot);
    // The outer router's layer runs before the inner routing, so the page's
    // token-free requests can reach the per-link handlers.
    Router::new()
        .fallback_service(links)
        .layer(axum::middleware::map_request(page_request))
}

/// The page runs at the token-free root and authenticates with the browser's
/// remembered link; the address bar never shows the key. Its relative requests
/// become the per-link routes. Without a usable cookie they name no link and
/// fail like an unknown one. Explicit `/u/<token>/` paths never use the cookie.
async fn page_request(mut request: axum::extract::Request) -> axum::extract::Request {
    let path = request.uri().path();
    let page_path = ["/api/", "/art/", "/assets/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || path == "/licenses";
    if !page_path {
        return request;
    }
    let token = remembered_link(request.headers())
        .flatten()
        .unwrap_or(Uuid::nil());
    let rewritten = match request.uri().query() {
        Some(query) => format!("/u/{token}{path}?{query}"),
        None => format!("/u/{token}{path}"),
    };
    if let Ok(uri) = rewritten.parse() {
        *request.uri_mut() = uri;
    }
    request
}

/// The link is a secret: no referrer, nothing from elsewhere, and nothing
/// cached except what a handler allows (cover art, the app's hashed files).
async fn headers(mut response: Response) -> Response {
    let set = [
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'none'; \
             connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
        (HeaderName::from_static("x-robots-tag"), "noindex"),
    ];
    let headers = response.headers_mut();
    for (name, value) in set {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// A failed API request: its status and `{"error": …}`.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.0, Json(Body { error: self.1 })).into_response()
    }
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

fn owner(bot: &Bot, token: &str) -> Result<Owner, ApiError> {
    Uuid::parse_str(token)
        .ok()
        .and_then(|token| bot.tokens.owner(&token))
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, INVALID_LINK.to_owned()))
}

/// Runs an admitted blocking call. Dropping or timing out its caller must not
/// release capacity while the real transport is still doing work.
async fn blocking_library_call<T: Send + 'static>(
    admission: &'static Semaphore,
    timeout: Duration,
    call: impl FnOnce() -> Result<T, subsonic::Error> + Send + 'static,
) -> Result<T, ApiError> {
    let permit = admission.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the music library is busy".to_owned(),
        )
    })?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        call()
    });
    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(result)) => {
            result.map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.public_message()))
        }
        Ok(Err(_)) => Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "the music library request failed".to_owned(),
        )),
        Err(_) => Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "the library did not answer in time".to_owned(),
        )),
    }
}

/// Runs a blocking library call with shared process-wide admission.
async fn ask<T: Send + 'static>(
    bot: &Bot,
    library: usize,
    call: impl FnOnce(&Library) -> Result<T, subsonic::Error> + Send + 'static,
) -> Result<T, ApiError> {
    if library >= bot.libraries.len() {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "no such library".to_owned(),
        ));
    }
    let libraries = bot.libraries.clone();
    blocking_library_call(&LIBRARY_CALLS, LIBRARY_TIMEOUT, move || {
        call(&libraries[library])
    })
    .await
}

/// The bot's public address: the page for a browser that opened its own link,
/// otherwise how to get one. Shared views keep their URL fragment.
async fn root(State(bot): State<Arc<Bot>>, headers: HeaderMap) -> Response {
    match remembered_link(&headers) {
        Some(Some(token)) if bot.tokens.is_valid(&token) => app_page(&bot).await,
        remembered => {
            let mut response = Html(
                "<!DOCTYPE html><meta charset=\"utf-8\"><title>Music</title>\
                 <p>Send <code>!web</code> to the music bot in Mumble to get your link to this page.\
                 <p>Open it once in this browser; afterwards shared links work here as well.",
            )
            .into_response();
            if remembered.is_some() {
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, link_cookie(&bot, None));
            }
            response
        }
    }
}

/// `None` without the cookie, `Some(None)` for a malformed value.
fn remembered_link(headers: &HeaderMap) -> Option<Option<Uuid>> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == LINK_COOKIE).then(|| Uuid::parse_str(value).ok())
        })
}

/// Remembers `token` (or forgets the link) below the configured public path.
fn link_cookie(bot: &Bot, token: Option<&Uuid>) -> HeaderValue {
    let public = &bot.config.web.public_url;
    let path = public
        .parse::<axum::http::Uri>()
        .ok()
        .map(|uri| uri.path().trim_end_matches('/').to_owned() + "/")
        .filter(|path| {
            path.bytes()
                .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b';' | b','))
        })
        .unwrap_or_else(|| "/".to_owned());
    let secure = if public
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        "; Secure"
    } else {
        ""
    };
    let (value, max_age) = match token {
        Some(token) => (token.to_string(), LINK_COOKIE_MAX_AGE),
        None => (String::new(), 0),
    };
    HeaderValue::from_str(&format!(
        "{LINK_COOKIE}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie built from a UUID and a checked path")
}

async fn slash(Path(token): Path<String>) -> Result<Redirect, ApiError> {
    // Path parameters are URL-decoded. Only a canonical UUID may become a
    // relative redirect, so encoded slashes cannot produce another origin.
    let token = Uuid::parse_str(&token)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, INVALID_LINK.into()))?;
    // Keep this relative: deployments may mount the bot below a proxy prefix.
    Ok(Redirect::permanent(&format!("{token}/")))
}

fn assets(bot: &Bot) -> Result<&PathBuf, ApiError> {
    bot.web_assets.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "The web page was not installed: start the bot with --web-assets.".to_owned(),
        )
    })
}

/// The link from `!web`: remember it in this browser, then show the page at the
/// token-free root so the address bar can be copied safely.
async fn remember(State(bot): State<Arc<Bot>>, Path(token): Path<String>) -> Response {
    let Ok(token) = Uuid::parse_str(&token)
        .map_err(drop)
        .and_then(|token| bot.tokens.is_valid(&token).then_some(token).ok_or(()))
    else {
        let page = format!(
            "<!DOCTYPE html><meta charset=\"utf-8\"><title>Unknown link</title><p>{INVALID_LINK}"
        );
        return (StatusCode::NOT_FOUND, Html(page)).into_response();
    };
    // Relative, like `slash`: the bot may be mounted below a proxy prefix.
    let mut response = Redirect::to("../../").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, link_cookie(&bot, Some(&token)));
    response
}

async fn app_page(bot: &Bot) -> Response {
    let directory = match assets(bot) {
        Ok(directory) => directory,
        Err(error) => return error.into_response(),
    };
    match tokio::fs::read_to_string(directory.join("index.html")).await {
        Ok(page) => Html(page).into_response(),
        Err(error) => {
            tracing::error!("web assets: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The web page is missing.",
            )
                .into_response()
        }
    }
}

/// Only the installed notice aggregate is served; no request selects a path.
async fn licenses(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    use tokio::io::AsyncReadExt;

    owner(&bot, &token)?;
    let missing = || {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "The licence notices were not installed.".to_owned(),
        )
    };
    let file = tokio::fs::File::open(assets(&bot)?.join("licenses/NOTICES.txt"))
        .await
        .map_err(|_| missing())?;
    const LIMIT: usize = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| missing())?;
    if bytes.is_empty() || bytes.len() > LIMIT || std::str::from_utf8(&bytes).is_err() {
        return Err(missing());
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response())
}

/// The type of an app file, by extension; others are not served.
fn asset_type(file: &str) -> Option<&'static str> {
    let (_, extension) = file.rsplit_once('.')?;
    Some(match extension {
        "js" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

/// The app's built files. Their names are content hashes, so they may be kept.
async fn asset(
    State(bot): State<Arc<Bot>>,
    Path((token, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let missing = || ApiError(StatusCode::NOT_FOUND, "no such file".to_owned());
    // Plain names only: nothing can reach outside the assets folder.
    let plain = !file.starts_with('.')
        && file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    let kind = asset_type(&file).filter(|_| plain).ok_or_else(missing)?;
    let directory = assets(&bot).map_err(|_| missing())?;
    let bytes = tokio::fs::read(directory.join("assets").join(&file))
        .await
        .map_err(|_| missing())?;
    Ok((
        [
            (header::CONTENT_TYPE, kind),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct ArtQuery {
    #[serde(default = "default_art_size")]
    size: u32,
    /// The library's `art_keys` entry from the state: proves the URL names
    /// the current library, so the browser may keep the answer.
    v: Option<String>,
}

/// The library's art key, cover ID and edge length.
type CoverKey = (String, String, u32);

/// Least recently used covers within a byte budget, each for at most `ttl`.
struct CoverCache {
    budget: usize,
    ttl: Duration,
    bytes: usize,
    tick: u64,
    entries: std::collections::HashMap<CoverKey, CachedCover>,
}

struct CachedCover {
    body: Bytes,
    stored: std::time::Instant,
    used: u64,
}

impl CachedCover {
    fn cost(&self, key: &CoverKey) -> usize {
        self.body.len() + key.0.len() + key.1.len()
    }
}

impl CoverCache {
    fn new(budget: usize, ttl: Duration) -> Self {
        Self {
            budget,
            ttl,
            bytes: 0,
            tick: 0,
            entries: std::collections::HashMap::new(),
        }
    }

    fn get(&mut self, key: &CoverKey, now: std::time::Instant) -> Option<Bytes> {
        let entry = self.entries.get(key)?;
        if now.saturating_duration_since(entry.stored) >= self.ttl {
            self.bytes -= entry.cost(key);
            self.entries.remove(key);
            return None;
        }
        self.tick += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.tick;
        Some(entry.body.clone())
    }

    fn insert(&mut self, key: CoverKey, body: Bytes, now: std::time::Instant) {
        let entry = CachedCover {
            body,
            stored: now,
            used: 0,
        };
        let cost = entry.cost(&key);
        // A few large covers must not push out everything else.
        if cost > self.budget / 8 {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.cost(&key);
        }
        let ttl = self.ttl;
        let mut freed = 0;
        self.entries.retain(|key, entry| {
            let fresh = now.saturating_duration_since(entry.stored) < ttl;
            if !fresh {
                freed += entry.cost(key);
            }
            fresh
        });
        self.bytes -= freed;
        while self.bytes + cost > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(gone) = self.entries.remove(&oldest) {
                self.bytes -= gone.cost(&oldest);
            }
        }
        self.tick += 1;
        self.bytes += cost;
        self.entries.insert(
            key,
            CachedCover {
                used: self.tick,
                ..entry
            },
        );
    }
}

fn default_art_size() -> u32 {
    300
}

#[derive(Serialize)]
struct CoverPixels {
    width: u32,
    height: u32,
    rgba: String,
}

fn content_worker_path(executable: &std::path::Path) -> Result<PathBuf, ApiError> {
    executable
        .parent()
        .and_then(|bin| bin.parent())
        .map(|root| root.join("libexec/nuschl-musicbot/nuschl-content-worker"))
        .ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the artwork worker is unavailable".to_owned(),
            )
        })
}

/// Only validated RGBA leaves this route. Never expose encoded library art to
/// the browser's native image decoder. The owned task retains concurrency
/// through worker completion/reaping even after its HTTP consumer disappears.
/// Answers come from the shared cache when they can; only misses take a job.
async fn art(
    State(bot): State<Arc<Bot>>,
    Path((token, library, id)): Path<(String, usize, String)>,
    Query(query): Query<ArtQuery>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let Some(source) = bot.libraries.get(library) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "no such library".to_owned(),
        ));
    };
    // A URL for another library (the configuration changed) must not be
    // kept by the browser under this library's covers.
    let versioned = match query.v.as_deref() {
        None => false,
        Some(key) if key == source.art_key() => true,
        Some(_) => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "the library changed; reload the page".to_owned(),
            ));
        }
    };
    let size = query.size.clamp(32, nuschl_ipc::MAX_THUMBNAIL_EDGE);
    let key: CoverKey = (source.art_key().to_owned(), id, size);
    let cached = COVERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key, std::time::Instant::now());
    let body = match cached {
        Some(body) => body,
        None => fetch_cover(&bot, library, key).await?,
    };
    owner(&bot, &token)?;
    let caching = if versioned {
        "private, max-age=86400"
    } else {
        "private, no-store"
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, caching),
        ],
        body,
    )
        .into_response())
}

/// Fetches and decodes a cover that is not cached, and caches it, even when
/// the HTTP client has gone by then.
async fn fetch_cover(bot: &Arc<Bot>, library: usize, key: CoverKey) -> Result<Bytes, ApiError> {
    let permit = COVER_JOBS.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the artwork worker is busy".to_owned(),
        )
    })?;
    let size = key.2;
    let id = key.1.clone();
    let worker = content_worker_path(&std::env::current_exe().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the artwork worker is unavailable".to_owned(),
        )
    })?)?;
    let owned = bot.clone();
    let task = tokio::spawn(async move {
        let _permit = permit;
        let image = ask(&owned, library, move |library| library.cover_art(&id, size))
            .await
            .map_err(|error| match error.0 {
                StatusCode::SERVICE_UNAVAILABLE => error,
                _ => ApiError(StatusCode::NOT_FOUND, error.1),
            })?;
        if !worker.is_file() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the artwork worker is unavailable".to_owned(),
            ));
        }
        let block = nuschl_ipc::host::thumbnail(&worker, &image.bytes, size, size)
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "the cover image is invalid or unavailable".to_owned(),
                )
            })?;
        let nuschl_core::ContentBlock::Image {
            width,
            height,
            rgba,
            source: _,
        } = block
        else {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "the artwork worker returned invalid pixels".to_owned(),
            ));
        };
        let body = Bytes::from(
            serde_json::to_vec(&CoverPixels {
                width,
                height,
                rgba,
            })
            .expect("cover pixels serialize"),
        );
        COVERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, body.clone(), std::time::Instant::now());
        Ok::<_, ApiError>(body)
    });
    match tokio::time::timeout(LIBRARY_TIMEOUT + Duration::from_secs(10), task).await {
        Ok(Ok(result)) => result,
        _ => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "the artwork worker did not finish in time".to_owned(),
        )),
    }
}

#[derive(Serialize)]
struct StateView {
    you: String,
    libraries: Vec<String>,
    /// Per library, parallel to `libraries`: whether it is known to offer
    /// sonic similarity. Read from what is already known, never asked.
    sonic: Vec<bool>,
    /// Per library: the `v` that lets the browser cache its covers.
    art_keys: Vec<String>,
    players: Vec<PlayerView>,
    channels: Vec<ChannelView>,
    rating_revision: u64,
}

#[derive(Serialize)]
struct PlayerView {
    name: String,
    paused: bool,
    volume: u8,
    ducking: DuckingView,
    ducked: bool,
    listeners: usize,
    error: Option<String>,
    current: Option<CurrentView>,
    queue: Vec<EntryView>,
    history_revision: u64,
}

#[derive(Serialize, Deserialize)]
struct DuckingView {
    enabled: bool,
    level: u8,
    hold_ms: u64,
    sensitivity: u8,
}

#[derive(Serialize)]
struct CurrentView {
    entry: EntryView,
    position_ms: u64,
    advancing: bool,
    buffering: bool,
}

#[derive(Serialize)]
struct EntryView {
    id: u64,
    added_by: String,
    track: Track,
}

#[derive(Serialize)]
struct ChannelView {
    name: String,
    server: String,
    connected: bool,
    channel: Option<String>,
    listeners: usize,
    error: Option<String>,
    player: usize,
}

/// Everything the app shows, as one snapshot.
fn snapshot(bot: &Bot, you: &str) -> StateView {
    let players = bot
        .players
        .iter()
        .map(|player| {
            let listeners = player.listeners();
            let state = player.state();
            let entry = |entry: &crate::player::Entry| EntryView {
                id: entry.id,
                added_by: entry.added_by.clone(),
                track: entry.track.clone(),
            };
            PlayerView {
                name: player.name.clone(),
                paused: state.paused,
                volume: state.volume,
                ducking: DuckingView {
                    enabled: state.ducking.enabled,
                    level: state.ducking.level,
                    hold_ms: state.ducking.hold.as_millis() as u64,
                    sensitivity: state.ducking.sensitivity,
                },
                ducked: player.ducked(&state.ducking),
                listeners,
                error: state.error.clone(),
                current: state.current.as_ref().map(|playing| CurrentView {
                    entry: entry(&playing.entry),
                    position_ms: playing.elapsed().as_millis() as u64,
                    advancing: !state.paused && listeners > 0 && !playing.buffering,
                    buffering: playing.buffering,
                }),
                queue: state.queue.iter().map(entry).collect(),
                history_revision: state.next_history_id.saturating_sub(1),
            }
        })
        .collect();
    let channels = bot
        .connections
        .iter()
        .map(|connection| {
            let status = connection.status().clone();
            ChannelView {
                name: connection.name.clone(),
                server: connection.server.clone(),
                connected: status.connected,
                channel: status.channel,
                listeners: status.listeners,
                error: status.error,
                player: *connection.player.borrow(),
            }
        })
        .collect();
    StateView {
        you: you.to_owned(),
        libraries: bot
            .libraries
            .iter()
            .map(|library| library.name.clone())
            .collect(),
        sonic: bot
            .libraries
            .iter()
            .map(|library| library.supports_sonic_similarity_cached())
            .collect(),
        art_keys: bot
            .libraries
            .iter()
            .map(|library| library.art_key().to_owned())
            .collect(),
        players,
        channels,
        rating_revision: bot.rating_revision.load(Ordering::SeqCst),
    }
}

async fn state(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Json<StateView>, ApiError> {
    let owner = owner(&bot, &token)?;
    Ok(Json(snapshot(&bot, &owner.name)))
}

#[derive(Deserialize)]
struct HistoryQuery {
    player: usize,
    before: Option<u64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct HistoryPage {
    items: Vec<HistoryView>,
    next_before: Option<u64>,
    oldest_id: Option<u64>,
    revision: u64,
}

#[derive(Serialize)]
struct HistoryView {
    id: u64,
    entry: HistoryEntryView,
    library_name: Option<String>,
    started_at_ms: Option<i64>,
    ended_at_ms: i64,
    played_ms: u64,
    position_ms: u64,
    outcome: crate::player::HistoryOutcome,
    error: Option<String>,
}

#[derive(Serialize)]
struct HistoryEntryView {
    id: u64,
    added_by: String,
    track: HistoryTrackView,
}

#[derive(Serialize)]
struct HistoryTrackView {
    library: Option<usize>,
    id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_id: Option<String>,
    artist_id: Option<String>,
    duration: Option<u32>,
    track: Option<u32>,
    suffix: Option<String>,
    content_type: Option<String>,
    cover_art: Option<String>,
}

fn history_view(
    item: &crate::player::HistoryItem,
    libraries: &[crate::config::Library],
) -> HistoryView {
    let track = &item.entry.track;
    let library = item.source.as_ref().and_then(|source| {
        libraries.iter().position(|library| {
            library.name == source.name && library.url.trim_end_matches('/') == source.url
        })
    });
    HistoryView {
        id: item.id,
        entry: HistoryEntryView {
            id: item.entry.id,
            added_by: item.entry.added_by.clone(),
            track: HistoryTrackView {
                library,
                id: track.id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                album_id: track.album_id.clone(),
                artist_id: track.artist_id.clone(),
                duration: track.duration,
                track: track.track,
                suffix: track.suffix.clone(),
                content_type: track.content_type.clone(),
                cover_art: track.cover_art.clone(),
            },
        },
        library_name: item.source.as_ref().map(|source| source.name.clone()),
        started_at_ms: item.started_at_ms,
        ended_at_ms: item.ended_at_ms,
        played_ms: item.played_ms,
        position_ms: item.position_ms,
        outcome: item.outcome,
        error: item.error.clone(),
    }
}

async fn history(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryPage>, ApiError> {
    owner(&bot, &token)?;
    let limit = query.limit.unwrap_or(50).min(100);
    if limit == 0 {
        return Err(bad("history limit must be greater than zero"));
    }
    let player = bot
        .players
        .get(query.player)
        .ok_or_else(|| bad("no such queue"))?;
    let state = player.state();
    let mut older = state
        .history
        .iter()
        .filter(|item| query.before.is_none_or(|before| item.id < before));
    let items: Vec<_> = older
        .by_ref()
        .take(limit)
        .map(|item| history_view(item, &bot.config.libraries))
        .collect();
    let next_before = if older.next().is_some() {
        items.last().map(|item| item.id)
    } else {
        None
    };
    Ok(Json(HistoryPage {
        items,
        next_before,
        oldest_id: state.history.back().map(|item| item.id),
        revision: state.next_history_id.saturating_sub(1),
    }))
}

#[derive(Deserialize)]
struct ListenQuery {
    player: usize,
}

/// An admitted body owns its producer; disconnecting cancels all listening work.
struct ListenBody {
    records: mpsc::Receiver<Bytes>,
    producer: tokio::task::JoinHandle<()>,
}

impl Stream for ListenBody {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.producer.is_finished() {
            self.records.close();
            return Poll::Ready(None);
        }
        self.records.poll_recv(cx).map(|record| record.map(Ok))
    }
}

impl Drop for ListenBody {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

/// Keeping the subscription and permit together prevents early capacity release
/// when a body is dropped but cancellation has not run yet.
struct ListenLease {
    subscription: BrowserSubscription,
    _permit: SemaphorePermit<'static>,
}

async fn listen(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ListenQuery>,
) -> Result<Response, ApiError> {
    listen_response(bot, token, query.player, &LISTEN_STREAMS)
}

fn listen_response(
    bot: Arc<Bot>,
    token: String,
    index: usize,
    admission: &'static Semaphore,
) -> Result<Response, ApiError> {
    let token = Uuid::parse_str(&token)
        .ok()
        .filter(|token| bot.tokens.is_valid(token))
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, INVALID_LINK.to_owned()))?;
    let player = bot.players.get(index).ok_or_else(|| bad("no such queue"))?;
    let permit = admission.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many browser listeners".to_owned(),
        )
    })?;
    let lease = ListenLease {
        subscription: player.subscribe_browser(),
        _permit: permit,
    };
    // Only one record may leave the shared live fanout before the body polls.
    // A separate task checks revocation even while HTTP backpressure stops polls.
    let (send, records) = mpsc::channel(1);
    let producer = tokio::spawn(async move {
        let mut lease = lease;
        let mut check = tokio::time::interval(Duration::from_secs(1));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_record = tokio::time::Instant::now();
        loop {
            if !bot.tokens.is_valid(&token) {
                break;
            }
            let space = tokio::select! {
                biased;
                _ = check.tick() => continue,
                result = send.reserve() => match result {
                    Ok(space) => space,
                    Err(_) => break,
                },
            };
            let record = tokio::select! {
                biased;
                _ = check.tick() => continue,
                _ = send.closed() => break,
                record = lease.subscription.recv() => match record {
                    Some(record) => record,
                    None => break,
                },
                _ = tokio::time::sleep_until(last_record + LISTEN_KEEPALIVE) => {
                    Bytes::from_static(KEEPALIVE_RECORD)
                }
            };
            if !bot.tokens.is_valid(&token) {
                break;
            }
            space.send(record);
            last_record = tokio::time::Instant::now();
        }
    });
    let body = Body::from_stream(ListenBody { records, producer });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
            (HeaderName::from_static("x-audio-format"), "f32le"),
            (HeaderName::from_static("x-audio-rate"), "48000"),
            (HeaderName::from_static("x-audio-channels"), "2"),
            (HeaderName::from_static("x-audio-frame-samples"), "960"),
        ],
        body,
    )
        .into_response())
}

/// The state now and after every change, until the link is revoked.
async fn events(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let permit = SSE_WATCHERS.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many live music viewers".to_owned(),
        )
    })?;
    let changes = bot.changes.subscribe();
    let stream = state_stream(bot, token, changes, permit);
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    // Proxies such as nginx must pass events on at once.
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

fn state_stream(
    bot: Arc<Bot>,
    token: String,
    changes: tokio::sync::watch::Receiver<u64>,
    permit: tokio::sync::SemaphorePermit<'static>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(
        (bot, token, changes, true, permit),
        |(bot, token, mut changes, first, permit)| async move {
            if !first {
                tokio::select! {
                    result = changes.changed() => { result.ok()?; }
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                tokio::time::sleep(COALESCE).await;
            }
            changes.borrow_and_update();
            let owner = owner(&bot, &token).ok()?;
            let event = Event::default()
                .event("state")
                .json_data(snapshot(&bot, &owner.name))
                .ok()?;
            Some((Ok(event), (bot, token, changes, false, permit)))
        },
    )
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Song,
    Album,
    Playlist,
    /// The songs that sound like `id`.
    Similar,
    /// The songs leading from `id` to `to` by sound.
    Path,
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Command {
    Pause {
        player: usize,
    },
    Resume {
        player: usize,
    },
    Skip {
        player: usize,
    },
    Clear {
        player: usize,
    },
    Seek {
        player: usize,
        position_ms: u64,
    },
    Volume {
        player: usize,
        volume: u8,
    },
    Ducking {
        player: usize,
        #[serde(flatten)]
        ducking: DuckingView,
    },
    Remove {
        player: usize,
        entry: u64,
    },
    Move {
        player: usize,
        entry: u64,
        index: usize,
    },
    Add {
        player: usize,
        kind: Kind,
        library: usize,
        id: String,
        /// The song a `path` ends with.
        to: Option<String>,
        index: Option<usize>,
    },
    Attach {
        channel: usize,
        player: usize,
    },
}

impl Command {
    fn player(&self) -> usize {
        match self {
            Self::Pause { player }
            | Self::Resume { player }
            | Self::Skip { player }
            | Self::Clear { player }
            | Self::Seek { player, .. }
            | Self::Volume { player, .. }
            | Self::Ducking { player, .. }
            | Self::Remove { player, .. }
            | Self::Move { player, .. }
            | Self::Add { player, .. }
            | Self::Attach { player, .. } => *player,
        }
    }
}

/// A change from the app; the new state reaches every viewer through `events`.
async fn command(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    // JSON only: a form on another site cannot send it without the browser asking first.
    Json(command): Json<Command>,
) -> Result<StatusCode, ApiError> {
    let owner = owner(&bot, &token)?;
    let player = bot
        .players
        .get(command.player())
        .ok_or_else(|| bad("no such queue"))?;
    match command {
        Command::Pause { .. } => player.set_paused(true),
        Command::Resume { .. } => player.set_paused(false),
        Command::Skip { .. } => player.skip(),
        Command::Clear { .. } => player.clear(),
        Command::Seek { position_ms, .. } => player.seek(Duration::from_millis(position_ms)),
        Command::Volume { volume, .. } => player.set_volume(volume),
        Command::Ducking { ducking, .. } => player.set_ducking(Ducking {
            enabled: ducking.enabled,
            level: ducking.level,
            hold: Duration::from_millis(ducking.hold_ms).min(MAX_DUCK_HOLD),
            sensitivity: ducking.sensitivity,
        }),
        Command::Remove { entry, .. } => player.remove(entry),
        Command::Move { entry, index, .. } => player.move_to(entry, index),
        Command::Add {
            kind,
            library,
            id,
            to,
            index,
            ..
        } => {
            let to = match kind {
                Kind::Path => to.ok_or_else(|| bad("a sonic path needs the song it ends with"))?,
                _ => String::new(),
            };
            let sonic = |matches: Vec<SonicMatch>| -> Vec<Track> {
                matches.into_iter().map(|found| found.track).collect()
            };
            let tracks = ask(&bot, library, move |library| match kind {
                Kind::Album => library.album(&id).map(|(_, songs)| songs),
                Kind::Playlist => library.playlist(&id).map(|(_, songs)| songs),
                Kind::Song => library.song(&id).map(|song| vec![song]),
                Kind::Similar => library.sonic_similar(&id).map(sonic),
                Kind::Path => library.sonic_path(&id, &to).map(sonic),
            })
            .await?;
            let tracks = match kind {
                Kind::Path => {
                    let identity = bot.libraries[library].identity();
                    let state = player.state();
                    let playing = state
                        .current
                        .as_ref()
                        .map(|playing| (playing.source.as_ref(), playing.entry.track.id.as_str()));
                    continue_path(tracks, playing, &identity)
                }
                _ => tracks,
            };
            let added = player.enqueue(tracks, &owner.name, index);
            tracing::info!("{} added {added} song(s) to {}", owner.name, player.name);
        }
        Command::Attach { channel, player } => {
            let connection = bot
                .connections
                .get(channel)
                .ok_or_else(|| bad("no such channel"))?;
            connection.player.send_replace(player);
            bot.changes.notify();
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A path that starts with the song the queue is playing continues it: the
/// first song is not queued again. `playing` is the current song's library
/// identity and id.
fn continue_path(
    mut path: Vec<Track>,
    playing: Option<(Option<&LibraryIdentity>, &str)>,
    library: &LibraryIdentity,
) -> Vec<Track> {
    if let (Some((Some(source), id)), Some(first)) = (playing, path.first())
        && source == library
        && id == first.id
    {
        path.remove(0);
    }
    path
}

#[derive(Deserialize)]
struct LibraryQuery {
    #[serde(default)]
    library: usize,
}

#[derive(Deserialize)]
struct ItemQuery {
    #[serde(default)]
    library: usize,
    id: String,
}

#[derive(Serialize)]
struct RatingView {
    rating: u8,
}

async fn rating(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<RatingView>, ApiError> {
    owner(&bot, &token)?;
    let rating = ask(&bot, query.library, move |library| {
        library.rating(&query.id)
    })
    .await?;
    Ok(Json(RatingView { rating }))
}

#[derive(Deserialize)]
struct RatingMutation {
    library: usize,
    id: String,
    rating: i64,
}

async fn set_rating(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    // JSON only, just like queue commands: cross-origin forms cannot submit it.
    Json(mutation): Json<RatingMutation>,
) -> Result<StatusCode, ApiError> {
    owner(&bot, &token)?;
    let rating = u8::try_from(mutation.rating)
        .ok()
        .filter(|rating| *rating <= 5)
        .ok_or_else(|| bad("rating must be between 0 and 5"))?;
    let changed = bot.clone();
    ask(&bot, mutation.library, move |library| {
        library.set_rating(&mutation.id, rating)?;
        // The admitted worker owns this notification: a cancelled or timed-out
        // HTTP caller cannot hide a backend write that actually succeeded.
        changed.rating_revision.fetch_add(1, Ordering::SeqCst);
        changed.changes.notify();
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AlbumsQuery {
    #[serde(default)]
    library: usize,
    #[serde(default)]
    list: String,
    #[serde(default)]
    page: u32,
}

#[derive(Serialize)]
struct AlbumPage {
    albums: Vec<Album>,
    more: bool,
}

async fn albums(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<AlbumsQuery>,
) -> Result<Json<AlbumPage>, ApiError> {
    owner(&bot, &token)?;
    let list = AlbumList::parse(&query.list).unwrap_or(AlbumList::Newest);
    let page = query.page.min(10_000);
    let albums = ask(&bot, query.library, move |library| {
        library.album_list(list, PAGE_SIZE, page * PAGE_SIZE)
    })
    .await?;
    // A random list is new every time; "more" would repeat albums.
    let more = list != AlbumList::Random && albums.len() as u32 == PAGE_SIZE;
    Ok(Json(AlbumPage { albums, more }))
}

#[derive(Serialize)]
struct AlbumView {
    album: Album,
    songs: Vec<Track>,
}

async fn album(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<AlbumView>, ApiError> {
    owner(&bot, &token)?;
    let (album, songs) = ask(&bot, query.library, move |library| library.album(&query.id)).await?;
    Ok(Json(AlbumView { album, songs }))
}

#[derive(Serialize)]
struct ArtistGroup {
    name: String,
    artists: Vec<Artist>,
}

#[derive(Serialize)]
struct ArtistIndex {
    groups: Vec<ArtistGroup>,
}

async fn artists(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> Result<Json<ArtistIndex>, ApiError> {
    owner(&bot, &token)?;
    let groups = ask(&bot, query.library, |library| library.artists()).await?;
    Ok(Json(ArtistIndex {
        groups: groups
            .into_iter()
            .map(|(name, artists)| ArtistGroup { name, artists })
            .collect(),
    }))
}

#[derive(Serialize)]
struct ArtistView {
    artist: Artist,
    albums: Vec<Album>,
}

async fn artist(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<ArtistView>, ApiError> {
    owner(&bot, &token)?;
    let (artist, albums) = ask(&bot, query.library, move |library| {
        library.artist(&query.id)
    })
    .await?;
    Ok(Json(ArtistView { artist, albums }))
}

#[derive(Serialize)]
struct PlaylistList {
    playlists: Vec<Playlist>,
}

async fn playlists(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> Result<Json<PlaylistList>, ApiError> {
    owner(&bot, &token)?;
    let playlists = ask(&bot, query.library, |library| library.playlists()).await?;
    Ok(Json(PlaylistList { playlists }))
}

#[derive(Serialize)]
struct PlaylistView {
    playlist: Playlist,
    songs: Vec<Track>,
}

async fn playlist(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<PlaylistView>, ApiError> {
    owner(&bot, &token)?;
    let (playlist, songs) = ask(&bot, query.library, move |library| {
        library.playlist(&query.id)
    })
    .await?;
    Ok(Json(PlaylistView { playlist, songs }))
}

#[derive(Serialize)]
struct SongView {
    song: Track,
    details: SongDetails,
    appearances: Vec<Track>,
    /// The song still shows when searching for its albums fails.
    appearances_error: Option<String>,
}

/// A song's page: its details and the albums it is on.
async fn song(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<SongView>, ApiError> {
    owner(&bot, &token)?;
    let view = ask(&bot, query.library, move |library| {
        let (song, details) = library.song_details(&query.id)?;
        let (appearances, appearances_error) = match library.appearances(&song) {
            Ok(appearances) => (appearances, None),
            Err(error) => (Vec::new(), Some(error.public_message())),
        };
        Ok(SongView {
            song,
            details,
            appearances,
            appearances_error,
        })
    })
    .await?;
    Ok(Json(view))
}

#[derive(Serialize)]
struct SimilarView {
    song: Track,
    matches: Vec<SonicMatch>,
}

/// The songs that sound like one song, and that song for the heading.
async fn similar(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<SimilarView>, ApiError> {
    owner(&bot, &token)?;
    let (song, matches) = ask(&bot, query.library, move |library| {
        // The extension check comes first: unsupported libraries get no other call.
        let matches = library.sonic_similar(&query.id)?;
        Ok((library.song(&query.id)?, matches))
    })
    .await?;
    Ok(Json(SimilarView { song, matches }))
}

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    library: usize,
    from: String,
    to: String,
}

#[derive(Serialize)]
struct SonicPathView {
    matches: Vec<SonicMatch>,
}

async fn sonic_path(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<PathQuery>,
) -> Result<Json<SonicPathView>, ApiError> {
    owner(&bot, &token)?;
    let matches = ask(&bot, query.library, move |library| {
        library.sonic_path(&query.from, &query.to)
    })
    .await?;
    Ok(Json(SonicPathView { matches }))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
struct SearchResults {
    results: Vec<LibraryResults>,
}

#[derive(Serialize)]
struct LibraryResults {
    library: usize,
    name: String,
    songs: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    error: Option<String>,
}

/// Searches any number of libraries, with at most eight calls in flight.
async fn search(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResults>, ApiError> {
    owner(&bot, &token)?;
    let text: String = query.q.trim().chars().take(MAX_QUERY).collect();
    if text.is_empty() {
        return Ok(Json(SearchResults {
            results: Vec::new(),
        }));
    }
    let searches = futures_util::stream::iter(0..bot.libraries.len())
        .map(|library| {
            let text = text.clone();
            let bot = &bot;
            async move {
                let result = ask(bot, library, move |library| library.search(&text)).await;
                match result {
                    // Reject overload promptly instead of waiting for the other
                    // searches; their permits stay with their blocking workers.
                    Err(error) if error.0 == StatusCode::SERVICE_UNAVAILABLE => Err(error),
                    result => Ok((library, result)),
                }
            }
        })
        .buffer_unordered(MAX_LIBRARY_CALLS);
    let mut found: Vec<_> = searches.try_collect().await?;
    found.sort_unstable_by_key(|(library, _)| *library);
    let mut results = Vec::with_capacity(found.len());
    for (library, result) in found {
        let name = bot.libraries[library].name.clone();
        results.push(match result {
            Ok(found) => LibraryResults {
                library,
                name,
                songs: found.songs,
                albums: found.albums,
                artists: found.artists,
                error: None,
            },
            Err(ApiError(_, error)) => LibraryResults {
                library,
                name,
                songs: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
                error: Some(error),
            },
        });
    }
    Ok(Json(SearchResults { results }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_redirect_is_canonical_and_cannot_escape_the_origin() {
        for token in ["//evil.example", "/evil", "https://evil.example", "bad"] {
            let response = slash(Path(token.into()))
                .await
                .err()
                .unwrap()
                .into_response();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        let response = slash(Path("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into()))
            .await
            .ok()
            .expect("valid UUID redirect")
            .into_response();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/"
        );
    }

    #[tokio::test]
    async fn the_page_runs_at_the_root_with_a_remembered_link_never_in_the_address() {
        let (directory, mut bot, token) = test_bot("http://unused.invalid", 0);
        std::fs::write(directory.path().join("index.html"), "<!DOCTYPE html><p>app").unwrap();
        Arc::get_mut(&mut bot).unwrap().web_assets = Some(directory.path().to_owned());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(bot.clone());
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // (status, Location, Set-Cookie, body) without following redirects.
        async fn get(
            url: String,
            cookie: Option<String>,
            body: Option<&'static str>,
        ) -> (u16, Option<String>, Option<String>, String) {
            tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .max_redirects(0)
                    .timeout_global(Some(Duration::from_secs(2)))
                    .build()
                    .into();
                let mut response = match body {
                    Some(body) => {
                        let mut request =
                            agent.post(&url).header("Content-Type", "application/json");
                        if let Some(cookie) = &cookie {
                            request = request.header("Cookie", cookie);
                        }
                        request.send(body).unwrap()
                    }
                    None => {
                        let mut request = agent.get(&url);
                        if let Some(cookie) = &cookie {
                            request = request.header("Cookie", cookie);
                        }
                        request.call().unwrap()
                    }
                };
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .map(|value| value.to_str().unwrap().to_owned())
                };
                let (location, set_cookie) = (header("location"), header("set-cookie"));
                let status = response.status().as_u16();
                let body = response.body_mut().read_to_string().unwrap();
                (status, location, set_cookie, body)
            })
            .await
            .unwrap()
        }
        let cookie = format!("other=1; {LINK_COOKIE}={token}; more=2");

        // The `!web` link only remembers itself and leaves for the root.
        let (status, location, set_cookie, _) = get(format!("{base}/u/{token}/"), None, None).await;
        assert_eq!((status, location.as_deref()), (303, Some("../../")));
        let set_cookie = set_cookie.unwrap();
        assert!(
            set_cookie.starts_with(&format!("{LINK_COOKIE}={token}; Path=/; ")),
            "{set_cookie}"
        );
        for attribute in ["HttpOnly", "SameSite=Lax", "Max-Age=7776000"] {
            assert!(set_cookie.contains(attribute), "{set_cookie}");
        }
        assert!(!set_cookie.contains("Secure"), "plain HTTP public URL");

        // The root shows the page only to the remembering browser.
        let (status, _, _, page) = get(format!("{base}/"), Some(cookie.clone()), None).await;
        assert_eq!((status, page.as_str()), (200, "<!DOCTYPE html><p>app"));
        let (status, _, set_cookie, page) = get(format!("{base}/"), None, None).await;
        assert!(status == 200 && page.contains("!web") && set_cookie.is_none());

        // Token-free page requests use the cookie; without it they are unknown.
        let (status, _, _, state) =
            get(format!("{base}/api/state"), Some(cookie.clone()), None).await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap()["you"],
            "Listener"
        );
        for missing in [None, Some(format!("{LINK_COOKIE}=not-a-uuid"))] {
            let (status, _, _, body) = get(format!("{base}/api/state"), missing, None).await;
            assert!(
                status == 404 && body.contains(INVALID_LINK),
                "{status} {body}"
            );
        }
        // Commands reach the handler (no queues here) only with the cookie.
        let command = r#"{"command":"pause","player":0}"#;
        assert_eq!(
            get(
                format!("{base}/api/command"),
                Some(cookie.clone()),
                Some(command)
            )
            .await
            .0,
            400
        );
        assert_eq!(
            get(format!("{base}/api/command"), None, Some(command))
                .await
                .0,
            404
        );
        // An explicit link in the path never falls back to the cookie.
        let nil = Uuid::nil();
        assert_eq!(
            get(
                format!("{base}/u/{nil}/api/state"),
                Some(cookie.clone()),
                None
            )
            .await
            .0,
            404
        );

        // Renewal revokes the remembered link: the root forgets it.
        let viewer = owner(&bot, &token).ok().unwrap();
        bot.tokens.issue(viewer, true).unwrap();
        let (status, _, set_cookie, page) =
            get(format!("{base}/"), Some(cookie.clone()), None).await;
        assert!(status == 200 && page.contains("!web"));
        let set_cookie = set_cookie.unwrap();
        assert!(
            set_cookie.starts_with(&format!("{LINK_COOKIE}=; Path=/; Max-Age=0")),
            "{set_cookie}"
        );
        assert_eq!(
            get(format!("{base}/api/state"), Some(cookie), None).await.0,
            404
        );
        let (status, _, set_cookie, _) = get(format!("{base}/u/{token}/"), None, None).await;
        assert!(status == 404 && set_cookie.is_none());
        serving.abort();
        let _ = serving.await;
    }

    #[test]
    fn the_remembered_link_stays_below_the_public_path() {
        let (_directory, mut bot, _) = test_bot("http://unused.invalid", 0);
        let token = Uuid::from_u128(7);
        for (public, path, secure) in [
            ("https://music.example.org/bot", "/bot/", true),
            ("HTTPS://music.example.org/bot/", "/bot/", true),
            ("http://music.example.org", "/", false),
            ("https://music.example.org/a;b", "/", true),
        ] {
            Arc::get_mut(&mut bot).unwrap().config.web.public_url = public.into();
            let cookie = link_cookie(&bot, Some(&token)).to_str().unwrap().to_owned();
            assert!(
                cookie.contains(&format!("; Path={path}; ")),
                "{public}: {cookie}"
            );
            assert_eq!(cookie.ends_with("; Secure"), secure, "{public}: {cookie}");
        }
    }
    use subsonic::tests::Server;

    // These API tests share the production admission pool; the overload test
    // deliberately occupies every permit and must not race other API consumers.
    static API_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn test_bot(url: &str, library_count: usize) -> (tempfile::TempDir, Arc<Bot>, String) {
        let directory = tempfile::tempdir().unwrap();
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "web": { "listen": "127.0.0.1:0", "public_url": "http://localhost/" },
            "subsonic": (0..library_count).map(|index| serde_json::json!({
                "name": format!("Library {index}"), "url": url,
            })).collect::<Vec<_>>(),
            "channel": [],
        }))
        .unwrap();
        let libraries = config
            .libraries
            .iter()
            .enumerate()
            .map(|(index, library)| {
                Library::new(
                    index,
                    library,
                    crate::config::Credentials::ApiKey("private-key".into()),
                )
            })
            .collect();
        let tokens = crate::tokens::Tokens::load(directory.path().join("tokens.json")).unwrap();
        let token = tokens
            .issue(
                Owner {
                    server: "mumble.example:64738".into(),
                    name: "Listener".into(),
                    certificate: Some("certificate".into()),
                },
                false,
            )
            .unwrap()
            .to_string();
        let bot = Arc::new(Bot {
            config,
            state_directory: directory.path().to_owned(),
            web_assets: None,
            codec_worker: std::path::PathBuf::from("/nonexistent/nuschl-musicbot-codec-worker"),
            libraries: Arc::new(libraries),
            players: Vec::new(),
            connections: Vec::new(),
            tokens,
            changes: crate::Changes::default(),
            persistence: std::sync::Mutex::new(()),
            rating_revision: std::sync::atomic::AtomicU64::new(0),
        });
        (directory, bot, token)
    }

    fn listening_bot() -> (tempfile::TempDir, Arc<Bot>, String) {
        let (directory, mut bot, token) = test_bot("http://unused.invalid", 0);
        let unique = Arc::get_mut(&mut bot).unwrap();
        unique.players.push(Arc::new(crate::player::Player::new(
            "Browser queue".into(),
            50,
            Ducking::default(),
            unique.changes.clone(),
        )));
        (directory, bot, token)
    }

    async fn released(bot: &Bot, count: usize) {
        let mut changes = bot.changes.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while bot.players[0].listeners() != count {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn listening_validates_links_and_queues_before_admission_and_bounds_live_bodies() {
        static ADMISSION: Semaphore = Semaphore::const_new(MAX_LISTEN_STREAMS);
        let (_directory, bot, token) = listening_bot();
        assert_eq!(
            listen_response(bot.clone(), "invalid".into(), 0, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            listen_response(bot.clone(), token.clone(), 1, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(ADMISSION.available_permits(), MAX_LISTEN_STREAMS);
        let mut responses = Vec::new();
        for _ in 0..MAX_LISTEN_STREAMS {
            responses.push(
                listen_response(bot.clone(), token.clone(), 0, &ADMISSION)
                    .ok()
                    .unwrap(),
            );
        }
        assert_eq!(bot.players[0].listeners(), MAX_LISTEN_STREAMS);
        assert_eq!(ADMISSION.available_permits(), 0);
        assert_eq!(
            listen_response(bot.clone(), token.clone(), 0, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            listen_response(bot.clone(), "invalid".into(), 0, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::NOT_FOUND,
            "auth precedes overload"
        );
        assert_eq!(
            listen_response(bot.clone(), token.clone(), 1, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::BAD_REQUEST,
            "queue validation precedes overload"
        );
        drop(responses.pop());
        released(&bot, MAX_LISTEN_STREAMS - 1).await;
        assert_eq!(ADMISSION.available_permits(), 1);
        responses.push(
            listen_response(bot.clone(), token, 0, &ADMISSION)
                .ok()
                .unwrap(),
        );
        drop(responses);
        released(&bot, 0).await;
        assert_eq!(ADMISSION.available_permits(), MAX_LISTEN_STREAMS);
    }

    #[tokio::test]
    async fn listening_emits_initial_reset_and_keeps_ownership_until_body_disconnects() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        let response = listen_response(bot.clone(), token, 0, &ADMISSION)
            .ok()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for (name, value) in [
            ("content-type", "application/octet-stream"),
            ("cache-control", "no-store"),
            ("x-accel-buffering", "no"),
            ("x-audio-format", "f32le"),
            ("x-audio-rate", "48000"),
            ("x-audio-channels", "2"),
            ("x-audio-frame-samples", "960"),
        ] {
            assert_eq!(response.headers()[name], value);
        }
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), &[0]);
        assert_eq!(
            ADMISSION.available_permits(),
            0,
            "headers do not release admission"
        );
        assert_eq!(bot.players[0].listeners(), 1);
        drop(stream);
        released(&bot, 0).await;
        assert_eq!(ADMISSION.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn revoked_idle_unpolled_listening_is_reaped_within_one_second() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        bot.players[0].set_paused(true);
        let response = listen_response(bot.clone(), token.clone(), 0, &ADMISSION)
            .ok()
            .unwrap();
        tokio::task::yield_now().await;
        let viewer = owner(&bot, &token).ok().unwrap();
        bot.tokens.issue(viewer, true).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        released(&bot, 0).await;
        assert_eq!(
            ADMISSION.available_permits(),
            1,
            "revocation drops the real lease even with HTTP backpressure"
        );
        let mut stream = response.into_body().into_data_stream();
        assert!(
            stream.next().await.is_none(),
            "revoked queued records are not delivered"
        );
        assert_eq!(
            listen_response(bot, token, 0, &ADMISSION).err().unwrap().0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_listening_sends_a_keepalive_record_at_least_every_fifteen_seconds() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        bot.players[0].set_paused(true);
        let response = listen_response(bot.clone(), token, 0, &ADMISSION)
            .ok()
            .unwrap();
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), &[0]);
        for _ in 0..3 {
            assert!(
                tokio::time::timeout(Duration::from_secs(14), stream.next())
                    .await
                    .is_err(),
                "a keep-alive came early"
            );
            let record = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("no keep-alive within 15 seconds")
                .unwrap()
                .unwrap();
            assert_eq!(record.as_ref(), KEEPALIVE_RECORD);
        }
        assert_eq!(ADMISSION.available_permits(), 0);
        drop(stream);
        released(&bot, 0).await;
        assert_eq!(ADMISSION.available_permits(), 1);
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn real_http_sse_cap_is_retained_until_the_stream_closes() {
        // The rating test also needs an SSE permit while this test fills the pool.
        let _exclusive = API_TESTS.lock().await;
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::{TcpListener, TcpStream},
        };
        async fn open(address: std::net::SocketAddr, token: &str) -> (TcpStream, String) {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket
                .write_all(
                    format!("GET /u/{token}/api/events HTTP/1.1\r\nHost: fixture\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut header = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 8192);
                    header.push(socket.read_u8().await.unwrap());
                }
            })
            .await
            .unwrap();
            (socket, String::from_utf8(header).unwrap())
        }
        let library = Server::reply("application/json", b"{}");
        let (_directory, bot, token) = test_bot(&library.url, 1);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            axum::serve(listener, router(bot)).await.unwrap();
        });
        let mut clients = Vec::new();
        for _ in 0..MAX_SSE_WATCHERS {
            let (socket, header) = open(address, &token).await;
            assert!(header.starts_with("HTTP/1.1 200"), "{header}");
            clients.push(socket);
        }
        assert_eq!(SSE_WATCHERS.available_permits(), 0);
        let (overload, header) = open(address, &token).await;
        assert!(header.starts_with("HTTP/1.1 503"), "{header}");
        drop(overload);
        // A completed handler did not release a live response body's permit.
        assert_eq!(SSE_WATCHERS.available_permits(), 0);
        drop(clients.pop());
        tokio::time::timeout(Duration::from_secs(5), async {
            while SSE_WATCHERS.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (replacement, header) = open(address, &token).await;
        assert!(header.starts_with("HTTP/1.1 200"), "{header}");
        clients.push(replacement);
        drop(clients);
        tokio::time::timeout(Duration::from_secs(5), async {
            while SSE_WATCHERS.available_permits() != MAX_SSE_WATCHERS {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        serving.abort();
        let _ = serving.await;
    }

    #[tokio::test]
    async fn rating_api_reads_sets_clears_and_only_publishes_successful_writes() {
        use std::{io::Write, sync::Mutex};

        let _exclusive = API_TESTS.lock().await;
        #[derive(Default)]
        struct Account {
            rating: Option<i64>,
            omit_rating: bool,
            fail: bool,
            calls: usize,
            writes: Vec<i64>,
        }
        let account = Arc::new(Mutex::new(Account {
            omit_rating: true,
            ..Account::default()
        }));
        let backend = Server::new({
            let account = account.clone();
            move |request, socket| {
                let target = request.split_whitespace().nth(1).unwrap();
                let (path, query) = target.split_once('?').unwrap();
                let parameter = |name: &str| {
                    query.split('&').find_map(|pair| {
                        let (key, value) = pair.split_once('=')?;
                        (key == name).then_some(value)
                    })
                };
                assert_eq!(parameter("apiKey"), Some("private-key"));
                assert_eq!(parameter("id"), Some("song"));
                let mut state = account.lock().unwrap();
                state.calls += 1;
                let reply = match path {
                    "/rest/getSong" => {
                        let mut song = serde_json::json!({"id": "song"});
                        if !state.omit_rating {
                            song["userRating"] = serde_json::json!(state.rating);
                        }
                        serde_json::json!({"status": "ok", "song": song})
                    }
                    "/rest/setRating" if state.fail => serde_json::json!({
                        "status": "failed", "error": {"code": 40,
                            "message": "http://private.example/?apiKey=private-key"}
                    }),
                    "/rest/setRating" => {
                        let rating = parameter("rating").unwrap().parse::<i64>().unwrap();
                        assert!((0..=5).contains(&rating));
                        state.writes.push(rating);
                        state.rating = (rating != 0).then_some(rating);
                        state.omit_rating = false;
                        serde_json::json!({"status": "ok"})
                    }
                    _ => panic!("unexpected backend endpoint"),
                };
                let body =
                    serde_json::to_vec(&serde_json::json!({"subsonic-response": reply})).unwrap();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let (_directory, bot, token) = test_bot(&backend.url, 1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/u/{token}/api", listener.local_addr().unwrap());
        let app = router(bot.clone());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });

        async fn request(url: String, body: Option<serde_json::Value>) -> (u16, serde_json::Value) {
            tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .timeout_global(Some(Duration::from_secs(2)))
                    .build()
                    .into();
                let mut response = match body {
                    Some(body) => agent.post(url).send_json(body).unwrap(),
                    None => agent.get(url).call().unwrap(),
                };
                let status = response.status().as_u16();
                let body = response.body_mut().read_to_vec().unwrap();
                (
                    status,
                    if body.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_slice(&body).unwrap()
                    },
                )
            })
            .await
            .unwrap()
        }
        let read_url = format!("{base}/rating?library=0&id=song");
        let write_url = format!("{base}/rating");
        let mutation = |library, rating| serde_json::json!({"library": library, "id": "song", "rating": rating});
        let missing_token = base.replace(&token, &Uuid::nil().to_string());
        assert_eq!(
            request(format!("{missing_token}/rating?library=0&id=song"), None)
                .await
                .0,
            404
        );
        assert_eq!(
            request(format!("{missing_token}/rating"), Some(mutation(0, 4)))
                .await
                .0,
            404
        );
        for invalid in [-1, 6] {
            assert_eq!(
                request(write_url.clone(), Some(mutation(0, invalid)))
                    .await
                    .0,
                400
            );
        }
        assert_eq!(
            request(write_url.clone(), Some(mutation(1, 4))).await.0,
            502
        );
        assert_eq!(
            request(format!("{base}/rating?library=1&id=song"), None)
                .await
                .0,
            502
        );
        assert_eq!(account.lock().unwrap().calls, 0);
        assert_eq!(
            request(read_url.clone(), None).await,
            (200, serde_json::json!({"rating": 0}))
        );
        account.lock().unwrap().omit_rating = false;
        assert_eq!(request(read_url.clone(), None).await.1["rating"], 0);

        // Consume the same SSE body the endpoint sends, checking the emitted
        // revision rather than merely an internal counter.
        let mut stream = events(State(bot.clone()), Path(token.clone()))
            .await
            .ok()
            .unwrap()
            .into_body()
            .into_data_stream();
        async fn event_revision(
            stream: &mut (impl Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
        ) -> u64 {
            let chunk = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let text = std::str::from_utf8(&chunk).unwrap();
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .or_else(|| text.lines().find_map(|line| line.strip_prefix("data:")))
                .unwrap();
            serde_json::from_str::<serde_json::Value>(data).unwrap()["rating_revision"]
                .as_u64()
                .unwrap()
        }
        assert_eq!(event_revision(&mut stream).await, 0);
        for (rating, revision) in [(4, 1), (0, 2)] {
            assert_eq!(
                request(write_url.clone(), Some(mutation(0, rating)))
                    .await
                    .0,
                204
            );
            assert_eq!(request(read_url.clone(), None).await.1["rating"], rating);
            assert_eq!(
                request(format!("{base}/state"), None).await.1["rating_revision"],
                revision
            );
            assert_eq!(event_revision(&mut stream).await, revision as u64);
        }
        account.lock().unwrap().fail = true;
        let failed = request(write_url, Some(mutation(0, 5))).await;
        assert_eq!(failed.0, 502);
        assert!(!failed.1.to_string().contains("private-key"));
        assert_eq!(request(read_url.clone(), None).await.1["rating"], 0);
        assert_eq!(
            request(format!("{base}/state"), None).await.1["rating_revision"],
            2
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err()
        );
        assert_eq!(account.lock().unwrap().writes, [4, 0]);
        for invalid in [-1, 6] {
            account.lock().unwrap().rating = Some(invalid);
            assert_eq!(request(read_url.clone(), None).await.0, 502);
        }
        account.lock().unwrap().rating = Some(3);
        assert_eq!(request(read_url, None).await.1["rating"], 3);
        drop(stream);
        stop.send(()).unwrap();
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_search_and_art_redact_remote_messages_and_reject_overload() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::reply("application/json", br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"http://dj:secret@private.example/rest/search3?apiKey=private-key"}}}"#);
        let (_directory, bot, token) = test_bot(&server.url, 9);
        let response = albums(
            State(bot.clone()),
            Path(token.clone()),
            Query(AlbumsQuery {
                library: 0,
                list: "newest".into(),
                page: 0,
            }),
        )
        .await
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response_json(response).await["error"],
            "the music library rejected the request (error 40)"
        );

        // More than eight configured libraries still all produce ordered,
        // independently redacted results; only actual parallel work is capped.
        let found = search(
            State(bot.clone()),
            Path(token.clone()),
            Query(SearchQuery { q: "song".into() }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(found.results.len(), 9);
        for (index, result) in found.results.iter().enumerate() {
            assert_eq!(result.library, index);
            assert_eq!(
                result.error.as_deref(),
                Some("the music library rejected the request (error 40)")
            );
        }
        assert!(
            !serde_json::to_string(&found)
                .unwrap()
                .contains("private-key")
        );

        let response = art(
            State(bot.clone()),
            Path((token.clone(), 0, "cover".into())),
            Query(ArtQuery { size: 300, v: None }),
        )
        .await
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json(response).await["error"],
            "the music library sent a non-image response"
        );

        let occupied = LIBRARY_CALLS
            .acquire_many(MAX_LIBRARY_CALLS as u32)
            .await
            .unwrap();
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            art(
                State(bot.clone()),
                Path((token.clone(), 0, "cover".into())),
                Query(ArtQuery { size: 300, v: None }),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response_json(response).await["error"],
            "the music library is busy"
        );
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            search(
                State(bot),
                Path(token),
                Query(SearchQuery { q: "song".into() }),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response_json(response).await["error"],
            "the music library is busy"
        );
        drop(occupied);
    }

    #[test]
    fn the_cover_cache_keeps_recent_covers_within_budget_and_age() {
        let start = std::time::Instant::now();
        let key = |id: &str| -> CoverKey { ("k".into(), id.into(), 300) };
        // Each entry costs its body plus the key text: 98 + 1 + 1 = 100 bytes.
        let body = || Bytes::from(vec![b'x'; 98]);
        let mut cache = CoverCache::new(800, Duration::from_secs(60));
        for id in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            cache.insert(key(id), body(), start);
        }
        assert_eq!(cache.bytes, 800);
        // Using "a" makes "b" the least recently used one.
        assert!(cache.get(&key("a"), start).is_some());
        cache.insert(key("i"), body(), start);
        assert!(cache.get(&key("b"), start).is_none());
        assert!(cache.get(&key("a"), start).is_some());
        assert_eq!(cache.bytes, 800);
        // Replacing an entry does not count it twice.
        cache.insert(key("a"), body(), start);
        assert_eq!((cache.bytes, cache.entries.len()), (800, 8));
        // More than an eighth of the budget is never cached.
        cache.insert(key("big"), Bytes::from(vec![0; 101]), start);
        assert!(cache.get(&key("big"), start).is_none());
        // Old entries are gone when read or when something new arrives.
        let later = start + Duration::from_secs(60);
        assert!(cache.get(&key("a"), later).is_none());
        cache.insert(key("j"), body(), later);
        assert_eq!((cache.bytes, cache.entries.len()), (100, 1));
    }

    #[tokio::test]
    async fn cached_covers_skip_the_library_and_only_versioned_urls_may_be_kept() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::reply(
            "application/json",
            br#"{"subsonic-response":{"status":"failed","error":{"code":0}}}"#,
        );
        let (_directory, bot, token) = test_bot(&server.url, 1);
        let art_key = bot.libraries[0].art_key().to_owned();
        assert_eq!(snapshot(&bot, "x").art_keys, [art_key.as_str()]);
        let body = Bytes::from_static(br#"{"width":1,"height":1,"rgba":"AAAAAA=="}"#);
        COVERS.lock().unwrap().insert(
            (art_key.clone(), "cover".into(), 300),
            body.clone(),
            std::time::Instant::now(),
        );
        // A hit needs no artwork job.
        let jobs = COVER_JOBS
            .acquire_many(MAX_COVER_JOBS as u32)
            .await
            .unwrap();
        let request = |v: Option<&str>| {
            art(
                State(bot.clone()),
                Path((token.clone(), 0, "cover".into())),
                Query(ArtQuery {
                    size: 300,
                    v: v.map(str::to_owned),
                }),
            )
        };
        for (v, caching) in [
            (Some(art_key.as_str()), "private, max-age=86400"),
            (None, "private, no-store"),
        ] {
            let response = request(v).await.ok().unwrap();
            assert_eq!(response.headers()[header::CACHE_CONTROL], caching);
            let served = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(served, body);
        }
        let stale = request(Some("0000000000000000")).await.err().unwrap();
        assert_eq!(stale.0, StatusCode::NOT_FOUND);
        // Not cached and no job free: the library is not asked either.
        let missing = art(
            State(bot.clone()),
            Path((token.clone(), 0, "other".into())),
            Query(ArtQuery { size: 300, v: None }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(missing.0, StatusCode::SERVICE_UNAVAILABLE);
        drop(jobs);
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn blocking_task_diagnostics_are_not_public_responses() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let response = blocking_library_call::<()>(&ADMISSION, LIBRARY_TIMEOUT, || {
            panic!("http://private.example/?apiKey=private-key")
        })
        .await
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response_json(response).await["error"],
            "the music library request failed"
        );
        let private = "http://private.example/?apiKey=private-key";
        for (error, expected) in [
            (
                subsonic::Error::Http(ureq::Error::BadUri(private.into())),
                "the music library request failed",
            ),
            (
                subsonic::Error::Status(500),
                "the music library returned HTTP status 500",
            ),
            (
                subsonic::Error::Api {
                    code: 40,
                    message: private.into(),
                },
                "the music library rejected the request (error 40)",
            ),
            (
                subsonic::Error::Json(serde_json::from_str::<String>("{}").unwrap_err()),
                "the music library returned an invalid reply",
            ),
            (
                subsonic::Error::NotAudio(private.into()),
                "the music library sent a non-audio response",
            ),
            (
                subsonic::Error::NotImage(private.into()),
                "the music library sent a non-image response",
            ),
            (
                subsonic::Error::Undecodable(private.into()),
                "the music library cannot send this song in a playable format",
            ),
        ] {
            let response =
                blocking_library_call::<()>(&ADMISSION, LIBRARY_TIMEOUT, move || Err(error))
                    .await
                    .err()
                    .unwrap()
                    .into_response();
            assert_eq!(response_json(response).await["error"], expected);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eight_real_workers_retain_admission_after_timeout_and_cancellation() {
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        static ADMISSION: Semaphore = Semaphore::const_new(MAX_LIBRARY_CALLS);
        struct Gate(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Gate {
            fn drop(&mut self) {
                let (open, changed) = &*self.0;
                *open.lock().unwrap() = true;
                changed.notify_all();
            }
        }
        let gate = Gate(Arc::new((Mutex::new(false), Condvar::new())));
        let (entered, mut entries) = tokio::sync::mpsc::channel(MAX_LIBRARY_CALLS);
        let mut callers = Vec::new();
        for index in 0..MAX_LIBRARY_CALLS {
            let gate = gate.0.clone();
            let entered = entered.clone();
            callers.push(tokio::spawn(blocking_library_call(
                &ADMISSION,
                if index < 4 {
                    Duration::from_secs(3600)
                } else {
                    Duration::ZERO
                },
                move || {
                    entered.blocking_send(()).unwrap();
                    let (open, changed) = &*gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = changed.wait(open).unwrap();
                    }
                    Ok(())
                },
            )));
        }
        for _ in 0..MAX_LIBRARY_CALLS {
            tokio::time::timeout(Duration::from_secs(2), entries.recv())
                .await
                .unwrap()
                .unwrap();
        }
        for (index, caller) in callers.into_iter().enumerate() {
            if index < 4 {
                caller.abort();
                assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
            } else {
                let response = caller.await.unwrap().err().unwrap().into_response();
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                assert_eq!(
                    response_json(response).await["error"],
                    "the library did not answer in time"
                );
            }
        }
        let started = Arc::new(AtomicUsize::new(0));
        let attempted = started.clone();
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            blocking_library_call(&ADMISSION, LIBRARY_TIMEOUT, move || {
                attempted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "busy calls must not spawn work"
        );

        drop(gate);
        let returned = tokio::time::timeout(
            Duration::from_secs(2),
            ADMISSION.acquire_many(MAX_LIBRARY_CALLS as u32),
        )
        .await
        .unwrap()
        .unwrap();
        drop(returned);
        assert!(
            blocking_library_call(&ADMISSION, LIBRARY_TIMEOUT, || Ok(()))
                .await
                .is_ok()
        );
    }

    #[test]
    fn historical_library_indices_never_alias_a_replacement() {
        let item: crate::player::HistoryItem = serde_json::from_value(serde_json::json!({
            "id": 1,
            "entry": { "id": 2, "added_by": "Listener", "track": {
                "library": 0, "id": "song", "title": "Historical title",
                "artist": "Historical artist", "album": "Historical album",
                "album_id": "album", "artist_id": "artist", "duration": 60,
                "track": 1, "suffix": "flac", "content_type": "audio/flac", "cover_art": "cover"
            } },
            "source": { "name": "Old library", "url": "https://old.example" },
            "started_at_ms": null, "ended_at_ms": 1000, "played_ms": 0,
            "position_ms": 0, "outcome": "failed", "error": "decode failed"
        }))
        .unwrap();
        let replacement: crate::config::Library = serde_json::from_value(serde_json::json!({
            "name": "New library", "url": "https://new.example"
        }))
        .unwrap();
        let removed = history_view(&item, std::slice::from_ref(&replacement));
        assert_eq!(removed.entry.track.library, None);
        let value = serde_json::to_value(&removed).unwrap();
        assert!(value.get("source").is_none());
        assert!(!value.to_string().contains("https://old.example"));
        let original: crate::config::Library = serde_json::from_value(serde_json::json!({
            "name": "Old library", "url": "https://old.example/"
        }))
        .unwrap();
        let reordered = history_view(&item, &[replacement, original]);
        assert_eq!(reordered.entry.track.library, Some(1));
    }

    #[test]
    fn only_plain_app_files_are_served() {
        assert_eq!(asset_type("index-Bq1x.js"), Some("text/javascript"));
        assert_eq!(asset_type("index-Bq1x.css"), Some("text/css"));
        assert_eq!(asset_type("secret.json"), None);
        assert_eq!(asset_type("noextension"), None);
    }

    #[test]
    fn commands_from_the_app() {
        let add: Command = serde_json::from_str(
            r#"{"command":"add","player":1,"kind":"album","library":0,"id":"a/1","index":null}"#,
        )
        .unwrap();
        assert!(matches!(
            add,
            Command::Add {
                kind: Kind::Album,
                index: None,
                player: 1,
                ..
            }
        ));
        let similar: Command = serde_json::from_str(
            r#"{"command":"add","player":0,"kind":"similar","library":1,"id":"s/1"}"#,
        )
        .unwrap();
        assert!(matches!(
            similar,
            Command::Add {
                kind: Kind::Similar,
                library: 1,
                to: None,
                index: None,
                ..
            }
        ));
        let path: Command = serde_json::from_str(
            r#"{"command":"add","player":0,"kind":"path","library":1,"id":"s1","to":"s 2","index":3}"#,
        )
        .unwrap();
        assert!(matches!(
            &path,
            Command::Add {
                kind: Kind::Path,
                to: Some(to),
                index: Some(3),
                ..
            } if to == "s 2"
        ));
        assert!(
            serde_json::from_str::<Command>(
                r#"{"command":"add","player":0,"kind":"sonic","library":0,"id":"s1"}"#
            )
            .is_err()
        );
        let ducking: Command = serde_json::from_str(
            r#"{"command":"ducking","player":0,"enabled":true,"level":20,"hold_ms":500,"sensitivity":70}"#,
        )
        .unwrap();
        assert!(matches!(
            ducking,
            Command::Ducking {
                ducking: DuckingView {
                    level: 20,
                    hold_ms: 500,
                    ..
                },
                ..
            }
        ));
        assert!(
            serde_json::from_str::<Command>(r#"{"command":"format_disk","player":0}"#).is_err()
        );
    }

    fn fixture_track(id: &str) -> Track {
        serde_json::from_value(serde_json::json!({ "library": 0, "id": id, "title": id })).unwrap()
    }

    #[test]
    fn a_path_from_the_playing_song_continues_it() {
        let library = LibraryIdentity {
            name: "Open".into(),
            url: "https://open.example".into(),
        };
        let other = LibraryIdentity {
            name: "Plain".into(),
            url: "https://plain.example".into(),
        };
        let ids = |tracks: Vec<Track>| tracks.into_iter().map(|track| track.id).collect::<Vec<_>>();
        let path = || vec![fixture_track("s1"), fixture_track("s2")];
        assert_eq!(
            ids(continue_path(
                path(),
                Some((Some(&library), "s1")),
                &library
            )),
            ["s2"]
        );
        for playing in [
            None,
            Some((None, "s1")),
            Some((Some(&other), "s1")),
            Some((Some(&library), "s2")),
        ] {
            assert_eq!(ids(continue_path(path(), playing, &library)), ["s1", "s2"]);
        }
        assert!(continue_path(Vec::new(), Some((Some(&library), "s1")), &library).is_empty());
    }

    /// `open` offers sonic similarity; `plain` does not; `failing` offers it
    /// but rejects every call with a message carrying the backend secret.
    fn sonic_fixture(mode: &'static str) -> Server {
        Server::new(move |request, socket| {
            use std::io::Write;
            let body = if request.starts_with("GET /rest/getOpenSubsonicExtensions") {
                if mode == "plain" {
                    r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"transcoding","versions":[1]}]}}"#
                } else {
                    r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"sonicSimilarity","versions":[1]}]}}"#
                }
            } else if mode == "failing" {
                r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"http://dj:secret@private.example/rest/findSonicPath?apiKey=private-key"}}}"#
            } else if request.starts_with("GET /rest/ping") {
                r#"{"subsonic-response":{"status":"ok"}}"#
            } else if request.starts_with("GET /rest/getSong") {
                r#"{"subsonic-response":{"status":"ok","song":{"id":"s1","title":"First"}}}"#
            } else if request.starts_with("GET /rest/getSonicSimilarTracks") {
                r#"{"subsonic-response":{"status":"ok","sonicMatch":[{"entry":{"id":"s2","title":"Second"},"similarity":0.8}]}}"#
            } else if request.starts_with("GET /rest/findSonicPath") {
                r#"{"subsonic-response":{"status":"ok","sonicMatch":[{"entry":{"id":"s1","title":"First"},"similarity":1.0},{"entry":{"id":"s2","title":"Second"},"similarity":0.5}]}}"#
            } else {
                r#"{"subsonic-response":{"status":"failed","error":{"code":0}}}"#
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(body.as_bytes()).unwrap();
        })
    }

    fn queue_ids(bot: &Bot) -> Vec<String> {
        bot.players[0]
            .state()
            .queue
            .iter()
            .map(|entry| entry.track.id.clone())
            .collect()
    }

    #[test]
    fn snapshot_reports_only_already_known_sonic_support() {
        let server = sonic_fixture("open");
        let (_directory, bot, token) = test_bot(&server.url, 2);
        let you = owner(&bot, &token).ok().unwrap().name;
        assert_eq!(snapshot(&bot, &you).sonic, [false, false]);
        assert!(server.requests().is_empty(), "snapshots never ask");
        bot.libraries[1].ping().unwrap();
        let asked = server.requests().len();
        // Library 0 would answer the same, but nobody has asked it yet.
        assert_eq!(snapshot(&bot, &you).sonic, [false, true]);
        assert_eq!(server.requests().len(), asked);
    }

    #[tokio::test]
    async fn a_song_page_lists_the_albums_with_the_same_song() {
        let _exclusive = API_TESTS.lock().await;
        for search_fails in [false, true] {
            let server = Server::new(move |request, socket| {
                use std::io::Write;
                let body = if request.starts_with("GET /rest/getSong") {
                    r#"{"subsonic-response":{"status":"ok","song":{"id":"s1","title":"Tide","artist":"Waves","album":"Sea","albumId":"a1","year":2021,"genre":"Ambient","bitRate":320,"discNumber":2,"playCount":7}}}"#
                } else if search_fails {
                    r#"{"subsonic-response":{"status":"failed","error":{"code":0,"message":"http://dj:secret@private.example/rest/search3"}}}"#
                } else {
                    // Same song on two other albums (one twice), a cover by
                    // someone else, a near title and a song without album.
                    r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[
                        {"id":"s1","title":"Tide","artist":"Waves","albumId":"a1"},
                        {"id":"s2","title":" tide ","artist":"WAVES","album":"Best of","albumId":"a2"},
                        {"id":"s3","title":"Tide","artist":"Waves","album":"Best of","albumId":"a2"},
                        {"id":"s4","title":"Tide","artist":"Someone","albumId":"a3"},
                        {"id":"s5","title":"Tide (Live)","artist":"Waves","albumId":"a4"},
                        {"id":"s6","title":"Tide","artist":"Waves"},
                        {"id":"s7","title":"Tide","artist":"Waves","album":"Live","albumId":"a5"}]}}}"#
                };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(body.as_bytes()).unwrap();
            });
            let (_directory, bot, token) = test_bot(&server.url, 1);
            let view = song(
                State(bot),
                Path(token),
                Query(ItemQuery {
                    library: 0,
                    id: "s1".into(),
                }),
            )
            .await
            .ok()
            .unwrap()
            .0;
            assert_eq!(view.song.title, "Tide");
            assert_eq!(
                view.details,
                SongDetails {
                    year: Some(2021),
                    genre: Some("Ambient".into()),
                    bit_rate: Some(320),
                    disc_number: Some(2),
                    play_count: Some(7),
                }
            );
            let albums: Vec<_> = view
                .appearances
                .iter()
                .map(|track| track.album_id.as_deref().unwrap())
                .collect();
            if search_fails {
                assert!(albums.is_empty());
                let error = view.appearances_error.unwrap();
                assert!(!error.contains("secret"), "{error}");
            } else {
                assert_eq!(albums, ["a1", "a2", "a5"]);
                assert_eq!(
                    view.appearances[0].id, "s1",
                    "the song's own album comes first"
                );
                assert!(view.appearances_error.is_none());
            }
        }
    }

    #[tokio::test]
    async fn sonic_lists_and_adds_follow_the_library_reply() {
        let _exclusive = API_TESTS.lock().await;
        let server = sonic_fixture("open");
        let (_directory, mut bot, token) = test_bot(&server.url, 2);
        let unique = Arc::get_mut(&mut bot).unwrap();
        unique.players.push(Arc::new(crate::player::Player::new(
            "Queue".into(),
            50,
            Ducking::default(),
            unique.changes.clone(),
        )));

        let found = similar(
            State(bot.clone()),
            Path(token.clone()),
            Query(ItemQuery {
                library: 1,
                id: "s1".into(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!((found.song.library, found.song.id.as_str()), (1, "s1"));
        let found = serde_json::to_value(&found).unwrap();
        assert_eq!(found["matches"][0]["track"]["id"], "s2");
        assert_eq!(found["matches"][0]["track"]["library"], 1);
        assert_eq!(found["matches"][0]["similarity"], 0.8);
        assert_eq!(found["matches"].as_array().unwrap().len(), 1);

        let path = sonic_path(
            State(bot.clone()),
            Path(token.clone()),
            Query(PathQuery {
                library: 1,
                from: "s1".into(),
                to: "s2".into(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(
            path.matches
                .iter()
                .map(|found| (found.track.id.as_str(), found.similarity))
                .collect::<Vec<_>>(),
            [("s1", Some(1.0)), ("s2", Some(0.5))]
        );
        let missing = sonic_path(
            State(bot.clone()),
            Path(token.clone()),
            Query(PathQuery {
                library: 2,
                from: "s1".into(),
                to: "s2".into(),
            }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            (missing.0, missing.1.as_str()),
            (StatusCode::BAD_GATEWAY, "no such library")
        );

        let add = |kind, to: Option<&str>| Command::Add {
            player: 0,
            kind,
            library: 1,
            id: "s1".into(),
            to: to.map(str::to_owned),
            index: None,
        };
        let asked = server.requests().len();
        let rejected = command(
            State(bot.clone()),
            Path(token.clone()),
            Json(add(Kind::Path, None)),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            server.requests().len(),
            asked,
            "rejected before the library"
        );
        command(
            State(bot.clone()),
            Path(token.clone()),
            Json(add(Kind::Similar, None)),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(queue_ids(&bot), ["s2"]);
        // Nothing plays, so the path is queued from its start.
        command(
            State(bot.clone()),
            Path(token.clone()),
            Json(add(Kind::Path, Some("s2"))),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(queue_ids(&bot), ["s2", "s1", "s2"]);
    }

    #[tokio::test]
    async fn sonic_endpoints_need_the_extension_redact_errors_and_reject_overload() {
        let _exclusive = API_TESTS.lock().await;
        async fn both(bot: &Arc<Bot>, token: &str) -> [ApiError; 2] {
            let similar = similar(
                State(bot.clone()),
                Path(token.to_owned()),
                Query(ItemQuery {
                    library: 0,
                    id: "s1".into(),
                }),
            );
            let path = sonic_path(
                State(bot.clone()),
                Path(token.to_owned()),
                Query(PathQuery {
                    library: 0,
                    from: "s1".into(),
                    to: "s2".into(),
                }),
            );
            let limit = Duration::from_secs(5);
            [
                tokio::time::timeout(limit, similar)
                    .await
                    .unwrap()
                    .err()
                    .unwrap(),
                tokio::time::timeout(limit, path)
                    .await
                    .unwrap()
                    .err()
                    .unwrap(),
            ]
        }

        let plain = sonic_fixture("plain");
        let (_directory, bot, token) = test_bot(&plain.url, 1);
        for error in both(&bot, &token).await {
            assert_eq!(
                (error.0, error.1.as_str()),
                (
                    StatusCode::BAD_GATEWAY,
                    "this music library does not support sonic similarity"
                )
            );
        }
        let requests = plain.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /rest/getOpenSubsonicExtensions"));

        let failing = sonic_fixture("failing");
        let (_directory, bot, token) = test_bot(&failing.url, 1);
        for error in both(&bot, &token).await {
            assert_eq!(
                (error.0, error.1.as_str()),
                (
                    StatusCode::BAD_GATEWAY,
                    "the music library rejected the request (error 40)"
                )
            );
        }
        assert!(
            failing
                .requests()
                .iter()
                .any(|request| request.starts_with("GET /rest/findSonicPath"))
        );

        let occupied = LIBRARY_CALLS
            .acquire_many(MAX_LIBRARY_CALLS as u32)
            .await
            .unwrap();
        let asked = failing.requests().len();
        for error in both(&bot, &token).await {
            assert_eq!(
                (error.0, error.1.as_str()),
                (StatusCode::SERVICE_UNAVAILABLE, "the music library is busy")
            );
        }
        assert_eq!(failing.requests().len(), asked);
        drop(occupied);
    }
}
