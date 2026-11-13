// SPDX-License-Identifier: EUPL-1.2
//! A client for the Subsonic API and the OpenSubsonic extensions it can use
//! (Navidrome, Gonic, Ampache, Airsonic and others). Calls block; async
//! callers run them with `spawn_blocking`.
use crate::config::{self, Credentials};
use md5::{Digest, Md5};
use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::HashSet, io::Read, sync::OnceLock, time::Duration};

const API_VERSION: &str = "1.16.1";
const CLIENT: &str = "nuschl-musicbot";
/// API replies are small; this bounds what a misbehaving server can make us hold.
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;
/// Metadata, art and transcode decisions have an end-to-end transport deadline.
pub(crate) const API_TIMEOUT: Duration = Duration::from_secs(30);
/// Songs have no total deadline, but a stalled transport read is finite.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// OpenSubsonic extension names (`getOpenSubsonicExtensions`).
const TRANSCODING: &str = "transcoding";
const SONIC_SIMILARITY: &str = "sonicSimilarity";
/// Songs asked for and kept from `getSonicSimilarTracks`.
pub const SONIC_SIMILAR_COUNT: usize = 25;
/// The most songs asked for and kept from `findSonicPath`, both ends included.
pub const SONIC_PATH_COUNT: usize = 25;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Http(#[from] ureq::Error),
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("the server answered: {message} (error {code})")]
    Api { code: i64, message: String },
    #[error("unexpected reply: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the server sent {0} instead of audio")]
    NotAudio(String),
    #[error("the server sent {0:?} instead of an image")]
    NotImage(String),
    #[error("the server cannot send this song in a format the bot plays: {0}")]
    Undecodable(String),
    #[error("the server does not offer the OpenSubsonic extension sonicSimilarity")]
    NoSonicSimilarity,
}

impl Error {
    /// Library errors reach durable history and authenticated viewers. Remote
    /// messages and transport errors may contain backend URLs or credentials.
    pub fn public_message(&self) -> String {
        match self {
            Self::Status(status) => format!("the music library returned HTTP status {status}"),
            Self::Api { code, .. } => {
                format!("the music library rejected the request (error {code})")
            }
            Self::Http(_) => "the music library request failed".into(),
            Self::Json(_) => "the music library returned an invalid reply".into(),
            Self::NotAudio(_) => "the music library sent a non-audio response".into(),
            Self::NotImage(_) => "the music library sent a non-image response".into(),
            Self::Undecodable(_) => {
                "the music library cannot send this song in a playable format".into()
            }
            Self::NoSonicSimilarity => {
                "this music library does not support sonic similarity".into()
            }
        }
    }
}

/// A song in one of the libraries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    /// Index of the library in the configuration.
    pub library: usize,
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
    /// In seconds.
    pub duration: Option<u32>,
    pub track: Option<u32>,
    /// What the file is, for the decoder (also saved with the queues).
    pub suffix: Option<String>,
    pub content_type: Option<String>,
    /// For `getCoverArt`.
    pub cover_art: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Album {
    pub library: usize,
    pub id: String,
    pub name: String,
    pub artist: Option<String>,
    pub artist_id: Option<String>,
    pub year: Option<u32>,
    pub song_count: Option<u32>,
    /// In seconds.
    pub duration: Option<u32>,
    pub cover_art: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Artist {
    pub library: usize,
    pub id: String,
    pub name: String,
    pub album_count: Option<u32>,
    pub cover_art: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Playlist {
    pub library: usize,
    pub id: String,
    pub name: String,
    pub song_count: Option<u32>,
    /// In seconds.
    pub duration: Option<u32>,
    pub owner: Option<String>,
    pub cover_art: Option<String>,
}

/// The album lists of `getAlbumList2` the web page offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlbumList {
    Newest,
    Recent,
    Frequent,
    Random,
    Alphabetical,
    Starred,
}

impl AlbumList {
    pub const ALL: [AlbumList; 6] = [
        Self::Newest,
        Self::Recent,
        Self::Frequent,
        Self::Random,
        Self::Alphabetical,
        Self::Starred,
    ];

    /// The `type` parameter, also used in the page's links.
    pub fn name(self) -> &'static str {
        match self {
            Self::Newest => "newest",
            Self::Recent => "recent",
            Self::Frequent => "frequent",
            Self::Random => "random",
            Self::Alphabetical => "alphabeticalByName",
            Self::Starred => "starred",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Newest => "Recently added",
            Self::Recent => "Recently played",
            Self::Frequent => "Most played",
            Self::Random => "Random",
            Self::Alphabetical => "By name",
            Self::Starred => "Starred",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|list| list.name() == name)
    }
}

/// Encoded library artwork; only the isolated content worker decodes it.
pub struct Image {
    pub bytes: Vec<u8>,
}

/// Cover art larger than this is refused.
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct SearchResult {
    pub songs: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
}

/// A song found by sonic similarity. The score belongs to this answer only and
/// never to the persisted `Track`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SonicMatch {
    pub track: Track,
    /// How close to the query song, 0 to 1; `None` when the server gives no
    /// usable score.
    pub similarity: Option<f64>,
}

/// The audio of a song as it arrives.
pub struct Stream {
    pub reader: Box<dyn Read + Send>,
    pub mime: Option<String>,
    /// The file name extension of what arrives, when known.
    pub extension: Option<String>,
}

/// Library identity is stable across configuration reordering and restarts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LibraryIdentity {
    pub name: String,
    pub url: String,
}

impl LibraryIdentity {
    pub fn new(library: &config::Library) -> Self {
        Self {
            name: library.name.clone(),
            url: library.url.trim_end_matches('/').to_owned(),
        }
    }
}

pub struct Library {
    index: usize,
    pub name: String,
    base: String,
    credentials: Credentials,
    transcode: Option<String>,
    max_bitrate: Option<u32>,
    api_agent: ureq::Agent,
    stream_agent: ureq::Agent,
    /// OpenSubsonic extensions, asked once the server answers.
    extensions: OnceLock<HashSet<String>>,
    /// Names this library in caches and shared links; a random value from
    /// the state directory that changes when the library's name or URL does.
    key: String,
}

impl Library {
    pub fn new(
        index: usize,
        config: &config::Library,
        credentials: Credentials,
        key: String,
    ) -> Self {
        let api_agent = transport_agent(Some(API_TIMEOUT), STREAM_READ_TIMEOUT);
        let stream_agent = transport_agent(None, STREAM_READ_TIMEOUT);
        let base = config.url.trim_end_matches('/').to_owned();
        Self {
            index,
            name: config.name.clone(),
            key,
            base,
            credentials,
            transcode: config.transcode.clone(),
            max_bitrate: config.max_bitrate,
            api_agent,
            stream_agent,
            extensions: OnceLock::new(),
        }
    }

    /// The library's key from `library_keys.json`, stable across restarts.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn identity(&self) -> LibraryIdentity {
        LibraryIdentity {
            name: self.name.clone(),
            url: self.base.clone(),
        }
    }

    fn url(&self, endpoint: &str) -> String {
        format!("{}/rest/{endpoint}", self.base)
    }

    /// Adds authentication and the common parameters. With a password, only a
    /// salted token is sent; with an API key, no username (which OpenSubsonic
    /// treats as conflicting authentication).
    fn authenticate<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        let request = match &self.credentials {
            Credentials::ApiKey(key) => request.query("apiKey", key),
            Credentials::Password { username, password } => {
                let salt = uuid::Uuid::new_v4().simple().to_string();
                let token = hex(&Md5::digest(format!("{password}{salt}")));
                request
                    .query("u", username)
                    .query("t", token)
                    .query("s", salt)
            }
        };
        request
            .query("v", API_VERSION)
            .query("c", CLIENT)
            .query("f", "json")
    }

    fn get(&self, endpoint: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        self.authenticate(self.api_agent.get(self.url(endpoint)))
    }

    fn stream_get(&self, endpoint: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        self.authenticate(self.stream_agent.get(self.url(endpoint)))
    }

    fn reply(mut response: ureq::http::Response<ureq::Body>) -> Result<Reply, Error> {
        if !response.status().is_success() {
            return Err(Error::Status(response.status().as_u16()));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_vec()?;
        parse_reply(&body)
    }

    fn call(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    ) -> Result<Reply, Error> {
        Self::reply(request.call()?)
    }

    pub fn ping(&self) -> Result<(), Error> {
        self.call(self.get("ping"))?;
        self.extensions();
        Ok(())
    }

    /// Announces playback or submits a finished play using its first audio's
    /// Unix timestamp in milliseconds. The configured library account owns it.
    /// During shutdown, `deadline` shortens the normal metadata timeout to the
    /// remaining grace; it never extends the standard 30-second limit.
    pub fn scrobble(
        &self,
        id: &str,
        time: i64,
        submission: bool,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), Error> {
        let request = self
            .get("scrobble")
            .query("id", id)
            .query("time", time.to_string())
            .query("submission", if submission { "true" } else { "false" });
        let request = if let Some(deadline) = deadline {
            let timeout = deadline
                .checked_duration_since(std::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(Error::Http(ureq::Error::Timeout(ureq::Timeout::Global)))?
                .min(API_TIMEOUT);
            request.config().timeout_global(Some(timeout)).build()
        } else {
            request
        };
        self.call(request)?;
        Ok(())
    }

    /// The server's OpenSubsonic extensions; empty for plain Subsonic servers.
    /// Asked again later while the server cannot be reached.
    fn extensions(&self) -> Option<&HashSet<String>> {
        if let Some(known) = self.extensions.get() {
            return Some(known);
        }
        let known = match self.call(self.get("getOpenSubsonicExtensions")) {
            Ok(reply) => reply
                .open_subsonic_extensions
                .into_iter()
                .map(|extension| extension.name)
                .collect(),
            // Plain Subsonic servers do not know the endpoint.
            Err(Error::Api { .. } | Error::Status(404) | Error::Json(_)) => HashSet::new(),
            Err(error) => {
                tracing::debug!("{}: OpenSubsonic extensions unknown: {error}", self.name);
                return None;
            }
        };
        if !known.is_empty() {
            tracing::info!(
                "{}: OpenSubsonic extensions: {}",
                self.name,
                known.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        Some(self.extensions.get_or_init(|| known))
    }

    fn has_extension(&self, name: &str) -> bool {
        self.extensions().is_some_and(|known| known.contains(name))
    }

    /// Whether the extensions already known include `sonicSimilarity`. Never
    /// asks the server; unknown extensions count as unsupported.
    pub fn supports_sonic_similarity_cached(&self) -> bool {
        self.extensions
            .get()
            .is_some_and(|known| known.contains(SONIC_SIMILARITY))
    }

    fn require_sonic_similarity(&self) -> Result<(), Error> {
        if self.has_extension(SONIC_SIMILARITY) {
            Ok(())
        } else {
            Err(Error::NoSonicSimilarity)
        }
    }

    /// Songs that sound like `id`, most similar first.
    pub fn sonic_similar(&self, id: &str) -> Result<Vec<SonicMatch>, Error> {
        self.require_sonic_similarity()?;
        let reply = self.call(
            self.get("getSonicSimilarTracks")
                .query("id", id)
                .query("count", SONIC_SIMILAR_COUNT.to_string()),
        )?;
        Ok(self.sonic_matches(reply, SONIC_SIMILAR_COUNT))
    }

    /// Songs leading from `from` to `to` by sound, both ends included.
    pub fn sonic_path(&self, from: &str, to: &str) -> Result<Vec<SonicMatch>, Error> {
        self.require_sonic_similarity()?;
        let reply = self.call(
            self.get("findSonicPath")
                .query("startSongId", from)
                .query("endSongId", to)
                .query("count", SONIC_PATH_COUNT.to_string()),
        )?;
        Ok(self.sonic_matches(reply, SONIC_PATH_COUNT))
    }

    fn sonic_matches(&self, reply: Reply, count: usize) -> Vec<SonicMatch> {
        reply
            .sonic_match
            .into_iter()
            .take(count)
            .map(|found| SonicMatch {
                track: found.entry.track(self.index),
                // `-1` means the server cannot score; anything outside 0..1 is unusable.
                similarity: found
                    .similarity
                    .filter(|similarity| (0.0..=1.0).contains(similarity)),
            })
            .collect()
    }

    pub fn search(&self, query: &str) -> Result<SearchResult, Error> {
        let reply = self.call(
            self.get("search3")
                .query("query", query)
                .query("songCount", "40")
                .query("albumCount", "12")
                .query("artistCount", "8"),
        )?;
        let found = reply.search_result3.unwrap_or_default();
        Ok(SearchResult {
            songs: found
                .song
                .into_iter()
                .map(|song| song.track(self.index))
                .collect(),
            albums: found
                .album
                .into_iter()
                .map(|album| album.album(self.index))
                .collect(),
            artists: found
                .artist
                .into_iter()
                .map(|artist| artist.artist(self.index))
                .collect(),
        })
    }

    pub fn song(&self, id: &str) -> Result<Track, Error> {
        self.song_details(id).map(|(track, _)| track)
    }

    /// A song with the details its page shows besides the queue fields.
    pub fn song_details(&self, id: &str) -> Result<(Track, SongDetails), Error> {
        let reply = self.call(self.get("getSong").query("id", id))?;
        let song = reply.song.ok_or_else(|| missing("song"))?;
        let details = SongDetails {
            year: song.year,
            genre: song.genre.clone(),
            bit_rate: song.bit_rate,
            disc_number: song.disc_number,
            play_count: song.play_count,
        };
        Ok((song.track(self.index), details))
    }

    /// One track per album with a song of the same title and artist as
    /// `song`, its own album first. Search finds candidates by title, the
    /// only field every server searches.
    pub fn appearances(&self, song: &Track) -> Result<Vec<Track>, Error> {
        let found = self.search(&song.title)?;
        Ok(same_song_albums(song, found.songs))
    }

    /// Reads the configured account's current rating; never use a queued snapshot.
    pub fn rating(&self, id: &str) -> Result<u8, Error> {
        let reply = self.call(self.get("getSong").query("id", id))?;
        let rating = reply
            .song
            .ok_or_else(|| missing("song"))?
            .user_rating
            .unwrap_or(0);
        u8::try_from(rating)
            .ok()
            .filter(|rating| *rating <= 5)
            .ok_or_else(|| {
                Error::Json(<serde_json::Error as serde::de::Error>::custom(
                    "song rating is outside 0..5",
                ))
            })
    }

    /// Writes the shared account's rating, with zero removing it. No retries.
    pub fn set_rating(&self, id: &str, rating: u8) -> Result<(), Error> {
        if rating > 5 {
            return Err(Error::Api {
                code: 0,
                message: "rating must be between 0 and 5".into(),
            });
        }
        self.call(
            self.get("setRating")
                .query("id", id)
                .query("rating", rating.to_string()),
        )?;
        Ok(())
    }

    pub fn album(&self, id: &str) -> Result<(Album, Vec<Track>), Error> {
        let reply = self.call(self.get("getAlbum").query("id", id))?;
        let album = reply.album.ok_or_else(|| missing("album"))?;
        let songs = album
            .song
            .into_iter()
            .map(|song| song.track(self.index))
            .collect();
        Ok((album.info.album(self.index), songs))
    }

    /// All artists, grouped by the server's index (usually initial letters).
    pub fn artists(&self) -> Result<Vec<(String, Vec<Artist>)>, Error> {
        let reply = self.call(self.get("getArtists"))?;
        let index = reply
            .artists
            .map(|artists| artists.index)
            .unwrap_or_default();
        Ok(index
            .into_iter()
            .map(|group| {
                let artists = group
                    .artist
                    .into_iter()
                    .map(|artist| artist.artist(self.index))
                    .collect();
                (group.name, artists)
            })
            .collect())
    }

    pub fn artist(&self, id: &str) -> Result<(Artist, Vec<Album>), Error> {
        let reply = self.call(self.get("getArtist").query("id", id))?;
        let artist = reply.artist.ok_or_else(|| missing("artist"))?;
        let albums = artist
            .album
            .into_iter()
            .map(|album| album.album(self.index))
            .collect();
        Ok((artist.info.artist(self.index), albums))
    }

    pub fn album_list(&self, list: AlbumList, size: u32, offset: u32) -> Result<Vec<Album>, Error> {
        let reply = self.call(
            self.get("getAlbumList2")
                .query("type", list.name())
                .query("size", size.to_string())
                .query("offset", offset.to_string()),
        )?;
        let albums = reply.album_list2.map(|list| list.album).unwrap_or_default();
        Ok(albums
            .into_iter()
            .map(|album| album.album(self.index))
            .collect())
    }

    pub fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        let reply = self.call(self.get("getPlaylists"))?;
        let playlists = reply
            .playlists
            .map(|list| list.playlist)
            .unwrap_or_default();
        Ok(playlists
            .into_iter()
            .map(|playlist| playlist.playlist(self.index))
            .collect())
    }

    pub fn playlist(&self, id: &str) -> Result<(Playlist, Vec<Track>), Error> {
        let reply = self.call(self.get("getPlaylist").query("id", id))?;
        let playlist = reply.playlist.ok_or_else(|| missing("playlist"))?;
        let songs = playlist
            .entry
            .into_iter()
            .map(|song| song.track(self.index))
            .collect();
        Ok((playlist.info.playlist(self.index), songs))
    }

    /// Cover art at most `size` pixels wide, if the server scales it. Only
    /// the common web image types are passed on.
    pub fn cover_art(&self, id: &str, size: u32) -> Result<Image, Error> {
        let mut response = self
            .get("getCoverArt")
            .query("id", id)
            .query("size", size.to_string())
            .call()?;
        if !response.status().is_success() {
            return Err(Error::Status(response.status().as_u16()));
        }
        let declared = response.body().mime_type().unwrap_or_default().to_owned();
        image_type(&declared).ok_or_else(|| Error::NotImage(declared.clone()))?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_IMAGE_BYTES)
            .read_to_vec()?;
        Ok(Image { bytes })
    }

    /// The audio of a song. A configured `transcode` format always applies;
    /// otherwise a server with the `transcoding` extension decides from what
    /// the bot decodes, and other servers send the original.
    pub fn stream(&self, track: &Track) -> Result<Stream, Error> {
        if self.transcode.is_none() && self.has_extension(TRANSCODING) {
            return self.decided_stream(track);
        }
        let mut request = self.stream_get("stream").query("id", &track.id);
        if let Some(format) = &self.transcode {
            request = request.query("format", format);
        }
        if let Some(rate) = self.max_bitrate {
            request = request.query("maxBitRate", rate.to_string());
        }
        let extension = match &self.transcode {
            Some(format) => Some(format.clone()),
            None => track.suffix.clone(),
        };
        self.audio(request.call()?, extension)
    }

    fn decided_stream(&self, track: &Track) -> Result<Stream, Error> {
        let request = self
            .authenticate(self.api_agent.post(self.url("getTranscodeDecision")))
            .query("mediaId", &track.id)
            .query("mediaType", "song");
        let reply = Self::reply(request.send_json(client_info(self.max_bitrate))?)?;
        let decision = reply
            .transcode_decision
            .ok_or_else(|| missing("transcodeDecision"))?;
        if decision.can_direct_play {
            let extension = decision
                .source_stream
                .and_then(|stream| stream.container)
                .or_else(|| track.suffix.clone());
            return self.audio(
                self.stream_get("stream").query("id", &track.id).call()?,
                extension,
            );
        }
        match decision.transcode_params {
            Some(params) if decision.can_transcode => {
                let extension = decision
                    .transcode_stream
                    .and_then(|stream| stream.container);
                let request = self
                    .stream_get("getTranscodeStream")
                    .query("mediaId", &track.id)
                    .query("mediaType", "song")
                    .query("transcodeParams", params);
                self.audio(request.call()?, extension)
            }
            _ => Err(Error::Undecodable(
                decision
                    .error_reason
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or_else(|| decision.transcode_reason.join(", ")),
            )),
        }
    }

    fn audio(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
        extension: Option<String>,
    ) -> Result<Stream, Error> {
        if !response.status().is_success() {
            return Err(Error::Status(response.status().as_u16()));
        }
        let mime = response.body().mime_type().map(str::to_owned);
        // Errors come as an API reply with a 200 status.
        if let Some(kind) = mime
            .as_deref()
            .filter(|mime| mime.ends_with("/json") || mime.ends_with("/xml"))
        {
            let body = response
                .body_mut()
                .with_config()
                .limit(MAX_REPLY_BYTES)
                .read_to_vec()?;
            parse_reply(&body)?;
            return Err(Error::NotAudio(kind.to_owned()));
        }
        Ok(Stream {
            reader: Box::new(response.into_body().into_reader()),
            mime,
            extension,
        })
    }
}

fn transport_agent(total_timeout: Option<Duration>, read_timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // Authentication lives in query parameters, which redirects can copy
        // even across an HTTPS downgrade. Require the canonical configured URL.
        .max_redirects(0)
        .timeout_global(total_timeout)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(API_TIMEOUT))
        .timeout_per_read(Some(read_timeout))
        .user_agent(CLIENT)
        .build()
        .new_agent()
}

/// What the decoder plays (`decode.rs`), for OpenSubsonic's transcode decision.
/// More than two channels are left to the server to mix down.
fn client_info(max_bitrate: Option<u32>) -> ClientInfo {
    let direct = |containers: &[&'static str], codecs: &[&'static str]| DirectPlayProfile {
        containers: containers.to_vec(),
        audio_codecs: codecs.to_vec(),
        protocols: vec!["http"],
        max_audio_channels: 2,
    };
    let transcode = |container, codec| TranscodingProfile {
        container,
        audio_codec: codec,
        protocol: "http",
        max_audio_channels: 2,
    };
    ClientInfo {
        name: CLIENT,
        platform: "Mumble",
        max_transcoding_audio_bitrate: max_bitrate.map_or(256_000, |kbit| kbit * 1000),
        direct_play_profiles: vec![
            direct(&["mp3"], &["mp3"]),
            direct(&["flac"], &["flac"]),
            direct(&["ogg", "oga"], &["vorbis", "opus", "flac"]),
            direct(&["opus"], &["opus"]),
            direct(&["wav"], &["pcm"]),
            direct(&["mp4", "m4a"], &["aac", "alac"]),
        ],
        transcoding_profiles: vec![transcode("ogg", "opus"), transcode("mp3", "mp3")],
    }
}

/// Library artwork MIME hints; bytes still require isolated format admission.
fn image_type(mime: &str) -> Option<&'static str> {
    [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/gif",
        "image/avif",
    ]
    .into_iter()
    .find(|known| mime.eq_ignore_ascii_case(known))
}

fn missing(what: &str) -> Error {
    Error::Api {
        code: 70,
        message: format!("the reply has no {what}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_reply(body: &[u8]) -> Result<Reply, Error> {
    let envelope: Envelope = serde_json::from_slice(body)?;
    let reply = envelope.response;
    if reply.status != "ok" {
        let error = reply.error.unwrap_or(ApiError {
            code: 0,
            message: format!("status {}", reply.status),
        });
        return Err(Error::Api {
            code: error.code,
            message: error.message,
        });
    }
    Ok(reply)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientInfo {
    name: &'static str,
    platform: &'static str,
    max_transcoding_audio_bitrate: u32,
    direct_play_profiles: Vec<DirectPlayProfile>,
    transcoding_profiles: Vec<TranscodingProfile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DirectPlayProfile {
    containers: Vec<&'static str>,
    audio_codecs: Vec<&'static str>,
    protocols: Vec<&'static str>,
    max_audio_channels: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscodingProfile {
    container: &'static str,
    audio_codec: &'static str,
    protocol: &'static str,
    max_audio_channels: u32,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "subsonic-response")]
    response: Reply,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    status: String,
    error: Option<ApiError>,
    search_result3: Option<SearchResult3>,
    song: Option<Song>,
    album: Option<AlbumWithSongs>,
    artists: Option<ArtistsIndex>,
    artist: Option<ArtistWithAlbums>,
    album_list2: Option<AlbumList2>,
    playlists: Option<Playlists>,
    playlist: Option<PlaylistWithSongs>,
    #[serde(default)]
    open_subsonic_extensions: Vec<Extension>,
    transcode_decision: Option<TranscodeDecision>,
    #[serde(default)]
    sonic_match: Vec<SonicMatchEntry>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct Extension {
    name: String,
}

#[derive(Deserialize)]
struct SonicMatchEntry {
    entry: Song,
    #[serde(default)]
    similarity: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscodeDecision {
    can_direct_play: bool,
    can_transcode: bool,
    #[serde(default)]
    transcode_reason: Vec<String>,
    error_reason: Option<String>,
    transcode_params: Option<String>,
    source_stream: Option<StreamDetails>,
    transcode_stream: Option<StreamDetails>,
}

#[derive(Deserialize)]
struct StreamDetails {
    container: Option<String>,
}

#[derive(Default, Deserialize)]
struct SearchResult3 {
    #[serde(default)]
    song: Vec<Song>,
    #[serde(default)]
    album: Vec<AlbumEntry>,
    #[serde(default)]
    artist: Vec<ArtistEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Song {
    #[serde(deserialize_with = "identifier")]
    id: String,
    #[serde(default)]
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    #[serde(default, deserialize_with = "optional_identifier")]
    album_id: Option<String>,
    #[serde(default, deserialize_with = "optional_identifier")]
    artist_id: Option<String>,
    duration: Option<u32>,
    track: Option<u32>,
    suffix: Option<String>,
    content_type: Option<String>,
    user_rating: Option<i64>,
    year: Option<u32>,
    genre: Option<String>,
    bit_rate: Option<u32>,
    disc_number: Option<u32>,
    play_count: Option<u64>,
    #[serde(default, deserialize_with = "optional_identifier")]
    cover_art: Option<String>,
}

/// What a song's page shows besides the queue fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SongDetails {
    pub year: Option<u32>,
    pub genre: Option<String>,
    /// In kbit/s.
    pub bit_rate: Option<u32>,
    pub disc_number: Option<u32>,
    pub play_count: Option<u64>,
}

fn same_song_albums(song: &Track, candidates: Vec<Track>) -> Vec<Track> {
    fn key(text: Option<&str>) -> Option<String> {
        text.map(|text| text.trim().to_lowercase())
    }
    let wanted = (key(Some(&song.title)), key(song.artist.as_deref()));
    let mut albums: Vec<Track> = song
        .album_id
        .is_some()
        .then(|| song.clone())
        .into_iter()
        .collect();
    for candidate in candidates {
        let same = (
            key(Some(&candidate.title)),
            key(candidate.artist.as_deref()),
        ) == wanted;
        let Some(album) = candidate.album_id.as_deref() else {
            continue;
        };
        if same
            && !albums
                .iter()
                .any(|known| known.album_id.as_deref() == Some(album))
        {
            albums.push(candidate);
        }
    }
    albums
}

impl Song {
    fn track(self, library: usize) -> Track {
        Track {
            library,
            title: self.title.unwrap_or_else(|| self.id.clone()),
            id: self.id,
            artist: self.artist,
            album: self.album,
            album_id: self.album_id,
            artist_id: self.artist_id,
            duration: self.duration,
            track: self.track,
            suffix: self.suffix,
            content_type: self.content_type,
            cover_art: self.cover_art,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumEntry {
    #[serde(deserialize_with = "identifier")]
    id: String,
    /// `name` in ID3 replies; older servers say `title`.
    name: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    #[serde(default, deserialize_with = "optional_identifier")]
    artist_id: Option<String>,
    year: Option<u32>,
    song_count: Option<u32>,
    duration: Option<u32>,
    #[serde(default, deserialize_with = "optional_identifier")]
    cover_art: Option<String>,
}

impl AlbumEntry {
    fn album(self, library: usize) -> Album {
        Album {
            library,
            name: self.name.or(self.title).unwrap_or_else(|| self.id.clone()),
            id: self.id,
            artist: self.artist,
            artist_id: self.artist_id,
            year: self.year,
            song_count: self.song_count,
            duration: self.duration,
            cover_art: self.cover_art,
        }
    }
}

#[derive(Deserialize)]
struct AlbumWithSongs {
    #[serde(flatten)]
    info: AlbumEntry,
    #[serde(default)]
    song: Vec<Song>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtistEntry {
    #[serde(deserialize_with = "identifier")]
    id: String,
    #[serde(default)]
    name: Option<String>,
    album_count: Option<u32>,
    #[serde(default, deserialize_with = "optional_identifier")]
    cover_art: Option<String>,
}

impl ArtistEntry {
    fn artist(self, library: usize) -> Artist {
        Artist {
            library,
            name: self.name.unwrap_or_else(|| self.id.clone()),
            id: self.id,
            album_count: self.album_count,
            cover_art: self.cover_art,
        }
    }
}

#[derive(Deserialize)]
struct ArtistsIndex {
    #[serde(default)]
    index: Vec<IndexGroup>,
}

#[derive(Deserialize)]
struct IndexGroup {
    #[serde(default)]
    name: String,
    #[serde(default)]
    artist: Vec<ArtistEntry>,
}

#[derive(Deserialize)]
struct ArtistWithAlbums {
    #[serde(flatten)]
    info: ArtistEntry,
    #[serde(default)]
    album: Vec<AlbumEntry>,
}

#[derive(Deserialize)]
struct AlbumList2 {
    #[serde(default)]
    album: Vec<AlbumEntry>,
}

#[derive(Deserialize)]
struct Playlists {
    #[serde(default)]
    playlist: Vec<PlaylistEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistEntry {
    #[serde(deserialize_with = "identifier")]
    id: String,
    #[serde(default)]
    name: Option<String>,
    song_count: Option<u32>,
    duration: Option<u32>,
    owner: Option<String>,
    #[serde(default, deserialize_with = "optional_identifier")]
    cover_art: Option<String>,
}

impl PlaylistEntry {
    fn playlist(self, library: usize) -> Playlist {
        Playlist {
            library,
            name: self.name.unwrap_or_else(|| self.id.clone()),
            id: self.id,
            song_count: self.song_count,
            duration: self.duration,
            owner: self.owner,
            cover_art: self.cover_art,
        }
    }
}

#[derive(Deserialize)]
struct PlaylistWithSongs {
    #[serde(flatten)]
    info: PlaylistEntry,
    #[serde(default)]
    entry: Vec<Song>,
}

fn optional_identifier<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    struct Wrapped(#[serde(deserialize_with = "identifier")] String);
    Ok(Option::<Wrapped>::deserialize(deserializer)?.map(|wrapped| wrapped.0))
}

/// Ids are strings in the specification, numbers on some older servers.
fn identifier<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Text(String),
        Number(u64),
    }
    Ok(match Id::deserialize(deserializer)? {
        Id::Text(text) => text,
        Id::Number(number) => number.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::Write,
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    /// A real HTTP peer, with close-delimited exchanges and bounded fixture I/O.
    pub(crate) struct Server {
        pub(crate) url: String,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        pub(crate) fn new(handler: impl Fn(&str, &mut TcpStream) + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let worker = thread::spawn({
                let requests = requests.clone();
                let stop = stop.clone();
                move || {
                    while !stop.load(Ordering::Acquire) {
                        let (mut socket, _) = match listener.accept() {
                            Ok(connection) => connection,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(1));
                                continue;
                            }
                            Err(error) => panic!("fixture accept: {error}"),
                        };
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut bytes = Vec::new();
                        while !bytes.ends_with(b"\r\n\r\n") {
                            let mut byte = [0];
                            socket.read_exact(&mut byte).unwrap();
                            bytes.push(byte[0]);
                            assert!(bytes.len() <= 128 * 1024, "fixture request too large");
                        }
                        let request = String::from_utf8(bytes).unwrap();
                        let length = request
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        assert!(length <= 128 * 1024, "fixture body too large");
                        socket.read_exact(&mut vec![0; length]).unwrap();
                        requests.lock().unwrap().push(request.clone());
                        handler(&request, &mut socket);
                    }
                }
            });
            Self {
                url,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        pub(crate) fn reply(mime: &'static str, body: &'static [u8]) -> Self {
            Self::new(move |_, socket| {
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(body).unwrap();
            })
        }

        pub(crate) fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn library(server: &Server) -> Library {
        let config = serde_json::from_value(serde_json::json!({
            "name": "Test library", "url": server.url,
        }))
        .unwrap();
        Library::new(
            0,
            &config,
            Credentials::ApiKey("private-key".into()),
            "key".into(),
        )
    }

    fn track() -> Track {
        parse_reply(
            br#"{"subsonic-response":{"status":"ok","song":{"id":"song","suffix":"flac"}}}"#,
        )
        .unwrap()
        .song
        .unwrap()
        .track(0)
    }

    #[test]
    fn authenticated_metadata_and_art_never_follow_redirects() {
        let server = Server::new(|request, socket| {
            if request.starts_with("GET /redirected") {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
            } else {
                socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /redirected?apiKey=private-key\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let library = library(&server);
        // Sonic calls only go out once the extension is known.
        library
            .extensions
            .set(HashSet::from([SONIC_SIMILARITY.into()]))
            .unwrap();
        let errors = [
            library.ping().err().unwrap(),
            library.search("song").err().unwrap(),
            library.song("song").err().unwrap(),
            library.album("album").err().unwrap(),
            library.artist("artist").err().unwrap(),
            library.artists().err().unwrap(),
            library.album_list(AlbumList::Newest, 1, 0).err().unwrap(),
            library.playlist("playlist").err().unwrap(),
            library.playlists().err().unwrap(),
            library.sonic_similar("song").err().unwrap(),
            library.sonic_path("song", "other").err().unwrap(),
            library.cover_art("cover", 300).err().unwrap(),
            library
                .scrobble("song", 1_700_000_000_123, false, None)
                .err()
                .unwrap(),
            library
                .scrobble("song", 1_700_000_000_123, true, None)
                .err()
                .unwrap(),
        ];
        assert!(
            errors
                .into_iter()
                .all(|error| matches!(error, Error::Status(302)))
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 14);
        assert!(
            requests
                .iter()
                .all(|request| request.contains("apiKey=private-key"))
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("GET /redirected"))
        );
    }

    #[test]
    fn authenticated_original_direct_and_transcoded_audio_never_follow_redirects() {
        for mode in 0..4 {
            let server = Server::new(move |request, socket| {
                if request.starts_with("POST /rest/getTranscodeDecision") {
                    let body = if mode == 2 {
                        br#"{"subsonic-response":{"status":"ok","transcodeDecision":{"canDirectPlay":true,"canTranscode":false}}}"#.as_slice()
                    } else {
                        br#"{"subsonic-response":{"status":"ok","transcodeDecision":{"canDirectPlay":false,"canTranscode":true,"transcodeParams":"params"}}}"#.as_slice()
                    };
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    socket.write_all(body).unwrap();
                } else if request.starts_with("GET /redirected") {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    socket.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /redirected?apiKey=private-key\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            });
            let mut library = library(&server);
            library
                .extensions
                .set(if mode >= 2 {
                    HashSet::from([TRANSCODING.into()])
                } else {
                    HashSet::new()
                })
                .unwrap();
            if mode == 1 {
                library.transcode = Some("mp3".into());
                library.credentials = Credentials::Password {
                    username: "dj".into(),
                    password: "private-password".into(),
                };
            }
            assert!(matches!(
                library.stream(&track()).err().unwrap(),
                Error::Status(307)
            ));
            let requests = server.requests();
            assert_eq!(requests.len(), if mode >= 2 { 2 } else { 1 });
            assert!(
                requests
                    .iter()
                    .all(|request| !request.contains("GET /redirected"))
            );
            assert!(requests.iter().all(|request| if mode == 1 {
                request.contains("u=dj")
                    && request.contains("t=")
                    && !request.contains("private-password")
            } else {
                request.contains("apiKey=private-key")
            }));
        }
    }

    #[test]
    fn transcode_decision_redirect_is_rejected_before_audio() {
        let server = Server::new(|_, socket| {
            socket.write_all(b"HTTP/1.1 308 Permanent Redirect\r\nLocation: http://127.0.0.1:1/?apiKey=private-key\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let library = library(&server);
        library
            .extensions
            .set(HashSet::from([TRANSCODING.into()]))
            .unwrap();
        assert!(matches!(
            library.stream(&track()).err().unwrap(),
            Error::Status(308)
        ));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /rest/getTranscodeDecision"));
    }

    #[test]
    fn metadata_art_and_decision_body_reads_obey_total_deadline() {
        for endpoint in ["search", "art", "decision", "scrobble", "scrobble-shutdown"] {
            let server = Server::new(|request, socket| {
                let (mime, body): (_, &[u8]) = if request.starts_with("GET /rest/getCoverArt") {
                    ("image/png", b"ab")
                } else {
                    ("application/json", br#"{"subsonic-response":{"status":"ok","transcodeDecision":{"canDirectPlay":true,"canTranscode":false}}}"#)
                };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body[..1]).unwrap();
                thread::sleep(Duration::from_millis(200));
                // A timed-out client may already have closed its socket.
                let _ = socket.write_all(&body[1..]);
            });
            let mut library = library(&server);
            if endpoint != "scrobble-shutdown" {
                library.api_agent =
                    transport_agent(Some(Duration::from_millis(20)), Duration::from_secs(1));
            }
            library
                .extensions
                .set(HashSet::from([TRANSCODING.into()]))
                .unwrap();
            let error = match endpoint {
                "search" => library.search("song").err().unwrap(),
                "art" => library.cover_art("cover", 300).err().unwrap(),
                "scrobble" => library
                    .scrobble("song", 1_700_000_000_123, true, None)
                    .err()
                    .unwrap(),
                "scrobble-shutdown" => library
                    .scrobble(
                        "song",
                        1_700_000_000_123,
                        true,
                        Some(std::time::Instant::now() + Duration::from_millis(20)),
                    )
                    .err()
                    .unwrap(),
                _ => library.stream(&track()).err().unwrap(),
            };
            assert!(
                matches!(
                    error,
                    Error::Http(ureq::Error::Timeout(ureq::Timeout::Global))
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn song_body_outlives_api_deadline_but_stalled_reads_fail() {
        for stalled in [false, true] {
            let server = Server::new(|_, socket| {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: 2\r\nConnection: close\r\n\r\na").unwrap();
                thread::sleep(Duration::from_millis(200));
                let _ = socket.write_all(b"b");
            });
            let mut library = library(&server);
            library.api_agent =
                transport_agent(Some(Duration::from_millis(20)), Duration::from_secs(1));
            library.stream_agent = transport_agent(
                None,
                if stalled {
                    Duration::from_millis(50)
                } else {
                    Duration::from_secs(1)
                },
            );
            library.extensions.set(HashSet::new()).unwrap();
            let mut stream = library.stream(&track()).unwrap();
            let mut bytes = Vec::new();
            let result = stream.reader.read_to_end(&mut bytes);
            if stalled {
                let error = result.unwrap_err();
                assert!(
                    matches!(
                        error
                            .get_ref()
                            .and_then(|error| error.downcast_ref::<ureq::Error>()),
                        Some(ureq::Error::Timeout(ureq::Timeout::PerRead))
                    ),
                    "{error}"
                );
            } else {
                result.unwrap();
                assert_eq!(bytes, b"ab");
            }
        }
    }

    #[test]
    fn search_reply_with_numeric_ids_and_missing_fields() {
        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1",
            "searchResult3":{"song":[{"id":17,"title":"Tango","duration":181},{"id":"a1"}],
            "album":[{"id":"x","title":"Old style","songCount":3}]}}}"#;
        let found = parse_reply(body).unwrap().search_result3.unwrap();
        let songs: Vec<_> = found.song.into_iter().map(|song| song.track(2)).collect();
        assert_eq!(songs[0].id, "17");
        assert_eq!(songs[0].duration, Some(181));
        assert_eq!(songs[1].title, "a1");
        assert_eq!(songs[1].library, 2);
        let album = found.album.into_iter().next().unwrap().album(0);
        assert_eq!(album.name, "Old style");
    }

    #[test]
    fn failed_reply_is_an_error() {
        let body = br#"{"subsonic-response":{"status":"failed","version":"1.16.1",
            "error":{"code":40,"message":"Wrong username or password"}}}"#;
        match parse_reply(body) {
            Err(Error::Api { code, message }) => {
                assert_eq!(code, 40);
                assert_eq!(message, "Wrong username or password");
            }
            other => panic!("expected an API error, got {:?}", other.err()),
        }
    }

    #[test]
    fn playback_errors_keep_codes_without_remote_urls_or_credentials() {
        let body = br#"{"subsonic-response":{"status":"failed","error":{
            "code":40,"message":"request http://dj:secret@private.example/rest/stream?apiKey=private-key failed"
        }}}"#;
        let error = parse_reply(body).err().unwrap();
        assert!(
            error.to_string().contains("private-key"),
            "retain diagnostics for operator logs"
        );
        assert_eq!(
            error.public_message(),
            "the music library rejected the request (error 40)"
        );
        let decision = Error::Undecodable("http://private.example/?password=secret".into());
        assert_eq!(
            decision.public_message(),
            "the music library cannot send this song in a playable format"
        );
    }

    #[test]
    fn transcode_decision_reply() {
        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1","openSubsonic":true,
            "transcodeDecision":{"canDirectPlay":false,"canTranscode":true,
            "transcodeReason":["AudioCodecNotSupported"],"errorReason":"",
            "transcodeParams":"0001-0005-004",
            "sourceStream":{"protocol":"http","container":"wma","codec":"wma"},
            "transcodeStream":{"protocol":"http","container":"ogg","codec":"opus"}}}}"#;
        let decision = parse_reply(body).unwrap().transcode_decision.unwrap();
        assert!(!decision.can_direct_play && decision.can_transcode);
        assert_eq!(decision.transcode_params.as_deref(), Some("0001-0005-004"));
        assert_eq!(
            decision.transcode_stream.unwrap().container.as_deref(),
            Some("ogg")
        );
    }

    #[test]
    fn album_reply_carries_its_details_beside_the_songs() {
        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1",
            "album":{"id":"al-1","name":"Waves","artist":"Oscillators","artistId":7,"year":2024,
            "songCount":2,"duration":360,"coverArt":"al-1_0",
            "song":[{"id":"s1","title":"Sine","track":1,"albumId":"al-1","coverArt":"al-1_0"}]}}}"#;
        let album = parse_reply(body).unwrap().album.unwrap();
        let info = album.info.album(1);
        assert_eq!(
            (info.name.as_str(), info.year, info.duration),
            ("Waves", Some(2024), Some(360))
        );
        assert_eq!(info.artist_id.as_deref(), Some("7"));
        assert_eq!(info.cover_art.as_deref(), Some("al-1_0"));
        let song = album.song.into_iter().next().unwrap().track(1);
        assert_eq!(
            (song.track, song.album_id.as_deref()),
            (Some(1), Some("al-1"))
        );
    }

    #[test]
    fn artist_index_artist_albums_and_playlists() {
        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1",
            "artists":{"ignoredArticles":"The","index":[{"name":"O","artist":[
            {"id":"ar-1","name":"Oscillators","albumCount":1,"coverArt":"ar-1_0"}]}]}}}"#;
        let index = parse_reply(body).unwrap().artists.unwrap().index;
        assert_eq!(index[0].name, "O");
        assert_eq!(index[0].artist[0].album_count, Some(1));

        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1",
            "artist":{"id":"ar-1","name":"Oscillators","album":[{"id":"al-1","name":"Waves"}]}}}"#;
        let artist = parse_reply(body).unwrap().artist.unwrap();
        assert_eq!(artist.info.artist(0).name, "Oscillators");
        assert_eq!(artist.album[0].id, "al-1");

        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1",
            "playlist":{"id":3,"name":"Mix","songCount":1,"owner":"ada",
            "entry":[{"id":"s2","title":"Other"}]}}}"#;
        let playlist = parse_reply(body).unwrap().playlist.unwrap();
        assert_eq!(playlist.info.playlist(0).id, "3");
        assert_eq!(playlist.entry.len(), 1);
    }

    fn json(socket: &mut TcpStream, body: &str) {
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        socket.write_all(body.as_bytes()).unwrap();
    }

    fn sonic_server() -> Server {
        Server::new(|request, socket| {
            let matches = |count: usize| {
                let scores = ["0.9", "-1", "null", "1.5", "1", "0"];
                (0..count)
                    .map(|index| {
                        let score = scores.get(index).copied().unwrap_or("0.5");
                        format!(r#"{{"entry":{{"id":{index},"title":"Song {index}"}},"similarity":{score}}}"#)
                    })
                    .chain([r#"{"entry":{"id":"unscored"}}"#.to_owned()])
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let body = if request.starts_with("GET /rest/getOpenSubsonicExtensions") {
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"sonicSimilarity","versions":[1]}]}}"#.to_owned()
            } else if request.starts_with("GET /rest/getSonicSimilarTracks") {
                format!(
                    r#"{{"subsonic-response":{{"status":"ok","sonicMatch":[{}]}}}}"#,
                    matches(SONIC_SIMILAR_COUNT + 4)
                )
            } else if request.starts_with("GET /rest/findSonicPath") {
                format!(
                    r#"{{"subsonic-response":{{"status":"ok","sonicMatch":[{}]}}}}"#,
                    matches(3)
                )
            } else {
                r#"{"subsonic-response":{"status":"failed","error":{"code":0}}}"#.to_owned()
            };
            json(socket, &body);
        })
    }

    #[test]
    fn sonic_replies_keep_order_truncate_and_only_keep_usable_scores() {
        let server = sonic_server();
        let config = serde_json::from_value(serde_json::json!({
            "name": "Sonic", "url": server.url,
        }))
        .unwrap();
        let library = Library::new(
            3,
            &config,
            Credentials::ApiKey("private-key".into()),
            "key".into(),
        );
        assert!(!library.supports_sonic_similarity_cached());

        let similar = library.sonic_similar("seed").unwrap();
        assert!(library.supports_sonic_similarity_cached());
        assert_eq!(similar.len(), SONIC_SIMILAR_COUNT);
        let ids: Vec<_> = similar
            .iter()
            .map(|found| found.track.id.as_str())
            .collect();
        assert_eq!(ids[..3], ["0", "1", "2"]);
        assert_eq!(ids[SONIC_SIMILAR_COUNT - 1], "24");
        assert!(similar.iter().all(|found| found.track.library == 3));
        let scores: Vec<_> = similar.iter().map(|found| found.similarity).collect();
        assert_eq!(
            scores[..7],
            [Some(0.9), None, None, None, Some(1.0), Some(0.0), Some(0.5)]
        );
        assert_eq!(
            serde_json::to_value(&similar[1]).unwrap(),
            serde_json::json!({
                "track": serde_json::to_value(&similar[1].track).unwrap(),
                "similarity": null,
            })
        );

        let path = library.sonic_path("from", "to").unwrap();
        let ids: Vec<_> = path.iter().map(|found| found.track.id.as_str()).collect();
        assert_eq!(ids, ["0", "1", "2", "unscored"]);
        assert_eq!(path[3].similarity, None);
        assert_eq!(path[3].track.title, "unscored");

        let requests = server.requests();
        assert_eq!(requests.len(), 3, "extensions are asked once");
        assert!(requests[1].starts_with("GET /rest/getSonicSimilarTracks?"));
        assert!(requests[1].contains("&id=seed&count=25 "));
        assert!(requests[2].starts_with("GET /rest/findSonicPath?"));
        assert!(requests[2].contains("&startSongId=from&endSongId=to&count=25 "));
    }

    #[test]
    fn sonic_calls_without_the_extension_never_reach_the_endpoints() {
        for answer in ["plain", "other", "unreachable"] {
            let server = Server::new(move |request, socket| {
                if !request.starts_with("GET /rest/getOpenSubsonicExtensions") {
                    json(
                        socket,
                        r#"{"subsonic-response":{"status":"ok","sonicMatch":[]}}"#,
                    );
                } else if answer == "plain" {
                    json(
                        socket,
                        r#"{"subsonic-response":{"status":"failed","error":{"code":70}}}"#,
                    );
                } else if answer == "other" {
                    json(
                        socket,
                        r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"transcoding","versions":[1]}]}}"#,
                    );
                } else {
                    socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            });
            let library = library(&server);
            for error in [
                library.sonic_similar("song").err().unwrap(),
                library.sonic_path("song", "other").err().unwrap(),
            ] {
                assert!(
                    matches!(error, Error::NoSonicSimilarity),
                    "{answer}: {error}"
                );
                assert_eq!(
                    error.public_message(),
                    "this music library does not support sonic similarity"
                );
            }
            assert!(!library.supports_sonic_similarity_cached());
            let requests = server.requests();
            assert!(
                requests
                    .iter()
                    .all(|request| request.starts_with("GET /rest/getOpenSubsonicExtensions")),
                "{answer}"
            );
            // Known answers are kept; an unreachable server is asked again.
            assert_eq!(
                requests.len(),
                if answer == "unreachable" { 2 } else { 1 },
                "{answer}"
            );
        }
    }

    #[test]
    fn only_web_image_types_pass() {
        assert_eq!(image_type("image/JPEG"), Some("image/jpeg"));
        assert_eq!(image_type("image/svg+xml"), None);
        assert_eq!(image_type("text/html"), None);
    }

    #[test]
    fn album_lists_round_trip_their_names() {
        for list in AlbumList::ALL {
            assert_eq!(AlbumList::parse(list.name()), Some(list));
        }
        assert_eq!(AlbumList::parse("byGenre"), None);
    }
}
