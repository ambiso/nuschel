// SPDX-License-Identifier: EUPL-1.2
//! The TOML configuration (see `nuschl-musicbot.example.toml`).
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub web: Web,
    #[serde(default)]
    pub audio: Audio,
    /// The music libraries; searches ask all of them.
    #[serde(rename = "subsonic")]
    pub libraries: Vec<Library>,
    /// One Mumble connection per entry. Each channel starts with its own queue.
    #[serde(rename = "channel")]
    pub channels: Vec<Channel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    pub listen: SocketAddr,
    /// Where users reach the web page; the links the bot hands out start with it.
    pub public_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Audio {
    /// The Opus bitrate in bits per second, lowered where a server allows less.
    pub bitrate: u32,
    /// The volume of a new queue, in percent.
    pub volume: u8,
    /// Whether queues start lowering the music while people talk.
    pub ducking: bool,
    /// The music's volume while people talk, in percent of its normal volume.
    pub duck_level: u8,
    /// How long the music stays lower after somebody stops talking.
    pub duck_hold_ms: u32,
    /// How readily sound counts as speech, in percent.
    pub duck_sensitivity: u8,
}

impl Default for Audio {
    fn default() -> Self {
        let ducking = crate::player::Ducking::default();
        Self {
            bitrate: 96_000,
            volume: 40,
            ducking: ducking.enabled,
            duck_level: ducking.level,
            duck_hold_ms: ducking.hold.as_millis() as u32,
            duck_sensitivity: ducking.sensitivity,
        }
    }
}

impl Audio {
    /// The ducking a new queue starts with.
    pub fn ducking(&self) -> crate::player::Ducking {
        crate::player::Ducking {
            enabled: self.ducking,
            level: self.duck_level,
            hold: std::time::Duration::from_millis(self.duck_hold_ms.into()),
            sensitivity: self.duck_sensitivity,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Library {
    /// Shown next to search results.
    pub name: String,
    /// The server's base URL, without `/rest`.
    pub url: String,
    /// An OpenSubsonic API key (`apiKeyAuthentication`), instead of a
    /// username and password.
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    /// Always ask the server to transcode to this format (Subsonic's `format`,
    /// e.g. `mp3`). Without it, servers with OpenSubsonic's `transcoding`
    /// extension transcode only what the bot cannot decode, and others send
    /// the original file.
    #[serde(default)]
    pub transcode: Option<String>,
    /// The highest bitrate a server transcodes to, in kbit/s.
    #[serde(default)]
    pub max_bitrate: Option<u32>,
}

/// How a library authenticates; read from files at startup.
pub enum Credentials {
    ApiKey(String),
    Password { username: String, password: String },
}

impl Library {
    pub fn credentials(&self) -> Result<Credentials, String> {
        let read = |path: &PathBuf| {
            std::fs::read_to_string(path)
                .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
                .map_err(|error| {
                    format!(
                        "subsonic library {}: {}: {error}",
                        self.name,
                        path.display()
                    )
                })
        };
        match (&self.api_key_file, &self.username, &self.password_file) {
            (Some(path), None, None) => Ok(Credentials::ApiKey(read(path)?)),
            (None, Some(username), Some(path)) => Ok(Credentials::Password {
                username: username.clone(),
                password: read(path)?,
            }),
            _ => Err(format!(
                "subsonic library {}: set either api_key_file, or username and password_file",
                self.name
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// A short identifier: the queue's name on the web page and the folder of
    /// this connection's client certificate.
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    /// The channel to join, as a path below the root channel (`Music` or
    /// `Games/Music`); empty for the root channel.
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    /// SHA-256 pin of the server's certificate, for self-signed servers.
    #[serde(default)]
    pub certificate_sha256: Option<String>,
    /// Access tokens (ACL group membership).
    #[serde(default)]
    pub tokens: Vec<String>,
    /// JSON array of access tokens, read only when connecting. Use a systemd
    /// credential instead of putting ACL secrets in the generated Nix store.
    #[serde(default)]
    pub tokens_file: Option<PathBuf>,
    /// Keep voice in the TLS connection instead of UDP.
    #[serde(default)]
    pub force_tcp: bool,
}

fn default_port() -> u16 {
    64738
}

impl Config {
    /// Parses and checks a configuration. Relative `*_file` paths are taken
    /// relative to `files` (systemd's credentials directory, or the
    /// configuration's own directory).
    pub fn parse(text: &str, files: &Path) -> Result<Self, String> {
        let mut config: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        config.validate()?;
        let resolve = |path: &mut Option<PathBuf>| {
            if let Some(path) = path {
                *path = files.join(&*path);
            }
        };
        for library in &mut config.libraries {
            resolve(&mut library.api_key_file);
            resolve(&mut library.password_file);
        }
        for channel in &mut config.channels {
            resolve(&mut channel.password_file);
            resolve(&mut channel.tokens_file);
        }
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        validate_url(&self.web.public_url, "web.public_url")?;
        if !(6_000..=510_000).contains(&self.audio.bitrate) {
            return Err("audio.bitrate must be between 6000 and 510000".into());
        }
        if self.audio.volume > 100 {
            return Err("audio.volume is a percentage from 0 to 100".into());
        }
        if self.audio.duck_level > 100 {
            return Err("audio.duck_level is a percentage from 0 to 100".into());
        }
        let longest = crate::player::MAX_DUCK_HOLD.as_millis();
        if u128::from(self.audio.duck_hold_ms) > longest {
            return Err(format!("audio.duck_hold_ms must be at most {longest}"));
        }
        if self.audio.duck_sensitivity > 100 {
            return Err("audio.duck_sensitivity is a percentage from 0 to 100".into());
        }
        if self.libraries.is_empty() {
            return Err("configure at least one [[subsonic]] library".into());
        }
        if self.channels.is_empty() {
            return Err("configure at least one [[channel]]".into());
        }
        for library in &self.libraries {
            validate_url(
                &library.url,
                &format!("subsonic library {}: url", library.name),
            )?;
            let password = library.username.is_some() && library.password_file.is_some();
            let partial = library.username.is_some() != library.password_file.is_some();
            if library.api_key_file.is_some() == password || partial {
                return Err(format!(
                    "subsonic library {}: set either api_key_file, or username and password_file",
                    library.name
                ));
            }
        }
        let mut names = HashSet::new();
        for channel in &self.channels {
            if channel.tokens_file.is_some() && !channel.tokens.is_empty() {
                return Err(format!("channel {}: set tokens or tokens_file, not both", channel.name));
            }
            validate_tokens(&channel.tokens)?;
            let valid = !channel.name.is_empty()
                && channel.name.len() <= 64
                && channel
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !valid {
                return Err(format!(
                    "channel name {:?}: use 1 to 64 letters, digits, - and _",
                    channel.name
                ));
            }
            if !names.insert(channel.name.as_str()) {
                return Err(format!("channel name {:?} is used twice", channel.name));
            }
            nuschl_session::config::ServerBookmark::new(
                &channel.name,
                &channel.host,
                channel.port,
                &channel.username,
            )
            .map_err(|error| format!("channel {}: {error}", channel.name))?;
            if let Some(pin) = &channel.certificate_sha256
                && (pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(format!(
                    "channel {}: certificate_sha256 must be 64 hexadecimal digits",
                    channel.name
                ));
            }
        }
        Ok(())
    }
}

impl Channel {
    pub fn access_tokens(&self) -> Result<Vec<String>, String> {
        use std::io::Read;
        const MAX_FILE: u64 = 128 * 1024;
        let Some(path) = &self.tokens_file else { return Ok(self.tokens.clone()) };
        let file = std::fs::File::open(path)
            .map_err(|_| format!("channel {}: could not read tokens_file", self.name))?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut bytes)
            .map_err(|_| format!("channel {}: could not read tokens_file", self.name))?;
        if bytes.len() as u64 > MAX_FILE {
            return Err("tokens_file exceeds 128 KiB".into());
        }
        let tokens: Vec<String> = serde_json::from_slice(&bytes)
            .map_err(|_| "tokens_file must contain a JSON array of strings".to_owned())?;
        validate_tokens(&tokens)?;
        Ok(tokens)
    }
}

fn validate_tokens(tokens: &[String]) -> Result<(), String> {
    if tokens.len() > 64 || tokens.iter().any(|token| token.len() > 256) {
        return Err("Mumble access tokens are limited to 64 entries of 256 bytes each".into());
    }
    Ok(())
}

/// Base addresses used to construct endpoint and authenticated-page URLs.
fn validate_url(value: &str, setting: &str) -> Result<(), String> {
    let valid = value.parse::<ureq::http::Uri>().ok().is_some_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri
                .authority()
                .is_some_and(|authority| !authority.host().is_empty())
            && uri
                .path_and_query()
                .is_none_or(|path| path.query().is_none())
            && !value.contains('#')
    });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{setting} must be an absolute http:// or https:// URL without a query or fragment"
        ))
    }
}

impl Channel {
    /// The path to the configured channel, without empty segments.
    pub fn path(&self) -> Vec<&str> {
        self.channel
            .split('/')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backed_acl_tokens_resolve_without_reading_during_validation() {
        let text = format!("{MINIMAL}\ntokens_file = \"acl\"\n");
        let mut config = parse(&text).unwrap();
        assert_eq!(config.channels[0].tokens_file.as_deref(), Some(Path::new("/run/credentials/musicbot/acl")));
        assert!(config.channels[0].access_tokens().is_err());
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("acl");
        std::fs::write(&file, br#"["first", "second"]"#).unwrap();
        config.channels[0].tokens_file = Some(file.clone());
        assert_eq!(config.channels[0].access_tokens().unwrap(), ["first", "second"]);
        std::fs::write(&file, br#"{"private":"must not enter an error"}"#).unwrap();
        let error = config.channels[0].access_tokens().unwrap_err();
        assert!(!error.contains("private"));
        std::fs::write(&file, serde_json::to_vec(&vec!["x"; 65]).unwrap()).unwrap();
        assert!(config.channels[0].access_tokens().is_err());
        let file = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        file.set_len(128 * 1024 + 1).unwrap();
        assert!(config.channels[0].access_tokens().is_err());
    }

    const MINIMAL: &str = r#"
        [web]
        listen = "127.0.0.1:8080"
        public_url = "https://music.example.org"

        [[subsonic]]
        name = "home"
        url = "https://navidrome.example.org"
        username = "bot"
        password_file = "home"

        [[channel]]
        name = "lounge"
        host = "voice.example.org"
        username = "Music"
        channel = " Games / Music/ "
    "#;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(text, Path::new("/run/credentials/musicbot"))
    }

    #[test]
    fn minimal_configuration_gets_defaults() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.channels[0].port, 64738);
        assert_eq!(config.channels[0].path(), ["Games", "Music"]);
        assert_eq!(config.audio.bitrate, 96_000);
    }

    #[test]
    fn relative_files_are_credentials() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(
            config.libraries[0].password_file.as_deref(),
            Some(Path::new("/run/credentials/musicbot/home"))
        );
        let absolute = MINIMAL.replace("\"home\"", "\"/etc/secret\"");
        let config = parse(&absolute).unwrap();
        assert_eq!(
            config.libraries[0].password_file.as_deref(),
            Some(Path::new("/etc/secret"))
        );
    }

    #[test]
    fn duplicate_and_invalid_channel_names_are_refused() {
        let twice =
            format!("{MINIMAL}\n[[channel]]\nname = \"lounge\"\nhost = \"a\"\nusername = \"b\"\n");
        assert!(parse(&twice).unwrap_err().contains("used twice"));
        let invalid = MINIMAL.replace("name = \"lounge\"", "name = \"../x\"");
        assert!(parse(&invalid).is_err());
    }

    #[test]
    fn a_library_needs_exactly_one_way_to_log_in() {
        let both = MINIMAL.replace(
            "username = \"bot\"",
            "username = \"bot\"\napi_key_file = \"key\"",
        );
        assert!(parse(&both).is_err());
        let key_only = MINIMAL
            .replace("username = \"bot\"\n", "")
            .replace("password_file = \"home\"", "api_key_file = \"key\"");
        assert!(parse(&key_only).is_ok());
        let half = MINIMAL.replace("password_file = \"home\"", "");
        assert!(parse(&half).is_err());
    }

    #[test]
    fn example_configuration_is_valid() {
        let config = parse(include_str!("../nuschl-musicbot.example.toml")).unwrap();
        assert_eq!(config.libraries.len(), 2);
        assert_eq!(config.channels[1].path(), ["Games", "Background"]);
    }
}
