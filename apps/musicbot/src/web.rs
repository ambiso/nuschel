// SPDX-License-Identifier: EUPL-1.2
//! The web page: a Svelte app (apps/musicbot/web) served below `/u/<token>/`,
//! a JSON API for it, and a stream of state changes (Server-Sent Events) so
//! that every viewer sees what anybody changes. All the app's addresses are
//! relative to the link, so it also works behind a proxy under a path. The
//! contract is described in docs/musicbot.md ("Web API").
use crate::{
    Bot,
    player::{Ducking, MAX_DUCK_HOLD},
    subsonic::{self, Album, AlbumList, Artist, Library, Playlist, Track},
    tokens::Owner,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{
        Html, IntoResponse, Redirect, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;

/// Searches longer than this are cut.
const MAX_QUERY: usize = 200;
/// How long a request waits for a library.
const LIBRARY_TIMEOUT: Duration = subsonic::API_TIMEOUT;
/// Count actual blocking calls, not callers still waiting for their replies.
const MAX_LIBRARY_CALLS: usize = 8;
static LIBRARY_CALLS: Semaphore = Semaphore::const_new(MAX_LIBRARY_CALLS);
/// Albums per page of an album list.
const PAGE_SIZE: u32 = 48;
/// Changes that come in a burst are sent as one state.
const COALESCE: Duration = Duration::from_millis(50);
const INVALID_LINK: &str =
    "This link is not valid (any more). Send !web to the music bot in Mumble for a new one.";

pub fn router(bot: Arc<Bot>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/u/{token}", get(slash))
        .route("/u/{token}/", get(app))
        .route("/u/{token}/assets/{file}", get(asset))
        .route("/u/{token}/licenses", get(licenses))
        .route("/u/{token}/art/{library}/{id}", get(art))
        .route("/u/{token}/api/state", get(state))
        .route("/u/{token}/api/history", get(history))
        .route("/u/{token}/api/events", get(events))
        .route("/u/{token}/api/command", post(command))
        .route("/u/{token}/api/albums", get(albums))
        .route("/u/{token}/api/album", get(album))
        .route("/u/{token}/api/artists", get(artists))
        .route("/u/{token}/api/artist", get(artist))
        .route("/u/{token}/api/playlists", get(playlists))
        .route("/u/{token}/api/playlist", get(playlist))
        .route("/u/{token}/api/search", get(search))
        .layer(axum::middleware::map_response(headers))
        .with_state(bot)
}

/// The link is a secret: no referrer, nothing from elsewhere, and nothing
/// cached except what a handler allows (cover art, the app's hashed files).
async fn headers(mut response: Response) -> Response {
    let set = [
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
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

async fn index() -> Html<&'static str> {
    Html(
        "<!DOCTYPE html><meta charset=\"utf-8\"><title>Music</title>\
         <p>Send <code>!web</code> to the music bot in Mumble to get your link to this page.",
    )
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

async fn app(State(bot): State<Arc<Bot>>, Path(token): Path<String>) -> Response {
    if owner(&bot, &token).is_err() {
        let page = format!(
            "<!DOCTYPE html><meta charset=\"utf-8\"><title>Unknown link</title><p>{INVALID_LINK}"
        );
        return (StatusCode::NOT_FOUND, Html(page)).into_response();
    }
    let directory = match assets(&bot) {
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
}

fn default_art_size() -> u32 {
    300
}

/// Cover art from a library, fetched with the bot's credentials. The browser
/// may keep it: the address holds the secret link, so the copy is private.
async fn art(
    State(bot): State<Arc<Bot>>,
    Path((token, library, id)): Path<(String, usize, String)>,
    Query(query): Query<ArtQuery>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let size = query.size.clamp(32, 1200);
    let image = ask(&bot, library, move |library| library.cover_art(&id, size))
        .await
        .map_err(|error| match error.0 {
            StatusCode::SERVICE_UNAVAILABLE => error,
            _ => ApiError(StatusCode::NOT_FOUND, error.1),
        })?;
    Ok((
        [
            (header::CONTENT_TYPE, image.mime),
            (header::CACHE_CONTROL, "private, max-age=86400"),
        ],
        image.bytes,
    )
        .into_response())
}

#[derive(Serialize)]
struct StateView {
    you: String,
    libraries: Vec<String>,
    players: Vec<PlayerView>,
    channels: Vec<ChannelView>,
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
        players,
        channels,
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

/// The state now and after every change, until the link is revoked.
async fn events(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let changes = bot.changes.subscribe();
    let stream = state_stream(bot, token, changes);
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
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(
        (bot, token, changes, true),
        |(bot, token, mut changes, first)| async move {
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
            Some((Ok(event), (bot, token, changes, false)))
        },
    )
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Song,
    Album,
    Playlist,
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
            index,
            ..
        } => {
            let tracks = ask(&bot, library, move |library| match kind {
                Kind::Album => library.album(&id).map(|(_, songs)| songs),
                Kind::Playlist => library.playlist(&id).map(|(_, songs)| songs),
                Kind::Song => library.song(&id).map(|song| vec![song]),
            })
            .await?;
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
            let response = slash(Path(token.into())).await.err().unwrap().into_response();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        let response = slash(Path("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into()))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/"
        );
    }
    use subsonic::tests::Server;

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
            libraries: Arc::new(libraries),
            players: Vec::new(),
            connections: Vec::new(),
            tokens,
            changes: crate::Changes::default(),
            persistence: std::sync::Mutex::new(()),
        });
        (directory, bot, token)
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn metadata_search_and_art_redact_remote_messages_and_reject_overload() {
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
            Query(ArtQuery { size: 300 }),
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
                Query(ArtQuery { size: 300 }),
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
}
