// SPDX-License-Identifier: EUPL-1.2
//! `link-previews.json`: the fetch policy, in a versioned sidecar beside
//! settings.json (whose `deny_unknown_fields` earlier versions would trip on).
//!
//! ```json
//! { "version": 1, "mode": "off" | "direct" | "proxy_required",
//!   "proxy": "socks5://127.0.0.1:9050",
//!   "dns": { "url": "https://dns.example/dns-query", "bootstrap": ["192.0.2.53"] } }
//! ```
//!
//! Reading fails closed: a missing file is Off; an unreadable, oversized,
//! malformed or unknown-version file is Off with a [`Problem`] to show, and
//! the caller keeps that file as it is. A ProxyRequired file stays
//! ProxyRequired even when its proxy settings are incomplete or invalid (its
//! route then fails, so nothing is fetched): no reading ever yields Direct
//! unless the file says Direct. A managed sidecar
//! (`managed-link-previews.json`, same format), when present, decides alone.
//!
//! No JSON parser is linked here: the caller passes a [`Codec`] (serde_json in
//! the frontends) and does the file I/O; this module owns the decisions.
use crate::policy::{Error as PolicyError, Mode, Settings};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "link-previews.json";
pub const MANAGED_FILE_NAME: &str = "managed-link-previews.json";
pub const VERSION: u32 = 1;
/// Larger files are not parsed.
pub const MAX_FILE_BYTES: usize = 4096;

/// A serde data format: serde_json for the real files.
pub trait Codec {
    type Error;
    fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, Self::Error>;
    fn encode<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, Self::Error>;
}

/// What reading the file gave.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    Missing,
    /// The file exists but could not be read (permissions, I/O, or longer
    /// than [`MAX_FILE_BYTES`] when the caller stopped reading early).
    Unreadable,
    Bytes(&'a [u8]),
}

/// Why the file was not used as written. No text from the file is kept: a
/// parser message could quote a proxy address or resolver URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Unreadable,
    TooLarge,
    Malformed,
    /// Written by a different (normally later) version.
    Version(u32),
    /// ProxyRequired without a complete, valid proxy and DNS configuration:
    /// previews stay off until it is fixed.
    Incomplete(PolicyError),
}

impl std::fmt::Display for Problem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable => write!(formatter, "{FILE_NAME} could not be read"),
            Self::TooLarge => write!(formatter, "{FILE_NAME} exceeds {MAX_FILE_BYTES} bytes"),
            Self::Malformed => write!(formatter, "{FILE_NAME} is not valid"),
            Self::Version(version) => {
                write!(formatter, "{FILE_NAME} has unknown version {version}")
            }
            Self::Incomplete(error) => write!(formatter, "proxy settings incomplete: {error}"),
        }?;
        formatter.write_str("; link previews are off")
    }
}

/// The policy read from one sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub settings: Settings,
    /// Whether a file was there at all (readable or not).
    pub present: bool,
    pub problem: Option<Problem>,
}

impl Loaded {
    fn off(present: bool, problem: Option<Problem>) -> Self {
        Self {
            settings: Settings::default(),
            present,
            problem,
        }
    }

    /// Whether saving may replace the file without the user explicitly
    /// choosing new settings: never over one that was not understood.
    pub fn may_overwrite(&self) -> bool {
        !matches!(
            self.problem,
            Some(
                Problem::Unreadable | Problem::TooLarge | Problem::Malformed | Problem::Version(_)
            )
        )
    }
}

#[derive(Deserialize)]
struct Probe {
    version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileV1 {
    version: u32,
    mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dns: Option<crate::policy::DnsSettings>,
}

/// Read one sidecar (the user's or the managed one).
pub fn parse<C: Codec>(codec: &C, source: Source<'_>) -> Loaded {
    let bytes = match source {
        Source::Missing => return Loaded::off(false, None),
        Source::Unreadable => return Loaded::off(true, Some(Problem::Unreadable)),
        Source::Bytes(bytes) => bytes,
    };
    if bytes.len() > MAX_FILE_BYTES {
        return Loaded::off(true, Some(Problem::TooLarge));
    }
    // The version first: a later version may have fields this one refuses.
    let Ok(probe) = codec.decode::<Probe>(bytes) else {
        return Loaded::off(true, Some(Problem::Malformed));
    };
    if probe.version != VERSION {
        return Loaded::off(true, Some(Problem::Version(probe.version)));
    }
    let Ok(file) = codec.decode::<FileV1>(bytes) else {
        return Loaded::off(true, Some(Problem::Malformed));
    };
    let settings = Settings {
        mode: file.mode,
        proxy: file.proxy,
        dns: file.dns,
    };
    let problem = match (settings.mode, settings.route()) {
        (Mode::ProxyRequired, Err(error)) => Some(Problem::Incomplete(error)),
        _ => None,
    };
    Loaded {
        settings,
        present: true,
        problem,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveError {
    Encode,
    TooLarge,
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode => write!(formatter, "{FILE_NAME} could not be written"),
            Self::TooLarge => write!(formatter, "{FILE_NAME} would exceed {MAX_FILE_BYTES} bytes"),
        }
    }
}
impl std::error::Error for SaveError {}

/// The file's bytes for `settings`. Incomplete proxy settings may be saved
/// (they read back as ProxyRequired with a [`Problem`]).
pub fn serialize<C: Codec>(codec: &C, settings: &Settings) -> Result<Vec<u8>, SaveError> {
    let file = FileV1 {
        version: VERSION,
        mode: settings.mode,
        proxy: settings.proxy.clone(),
        dns: settings.dns.clone(),
    };
    let bytes = codec.encode(&file).map_err(|_| SaveError::Encode)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(SaveError::TooLarge);
    }
    Ok(bytes)
}

/// The policy in force: a present managed sidecar decides alone, and one
/// that cannot be used means Off (never the user's Direct); otherwise the
/// user's sidecar.
pub fn effective(user: &Loaded, managed: &Loaded) -> Loaded {
    // `parse` already made a managed file with a problem Off, or kept it an
    // incomplete ProxyRequired.
    if managed.present {
        managed.clone()
    } else {
        user.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{DnsSettings, Route};

    struct Json;
    impl Codec for Json {
        type Error = serde_json::Error;
        fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, Self::Error> {
            serde_json::from_slice(bytes)
        }
        fn encode<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, Self::Error> {
            serde_json::to_vec_pretty(value)
        }
    }

    fn read(text: &str) -> Loaded {
        parse(&Json, Source::Bytes(text.as_bytes()))
    }

    fn proxy_settings() -> Settings {
        Settings {
            mode: Mode::ProxyRequired,
            proxy: Some("socks5://127.0.0.1:9050".into()),
            dns: Some(DnsSettings {
                url: "https://dns.example/dns-query".into(),
                bootstrap: vec![
                    "192.0.2.53".parse().unwrap(),
                    "2001:db8::53".parse().unwrap(),
                ],
            }),
        }
    }

    #[test]
    fn every_mode_round_trips() {
        for settings in [
            Settings::default(),
            Settings {
                mode: Mode::Direct,
                ..Settings::default()
            },
            proxy_settings(),
        ] {
            let bytes = serialize(&Json, &settings).unwrap();
            let loaded = parse(&Json, Source::Bytes(&bytes));
            assert_eq!(loaded.settings, settings);
            assert!(loaded.present);
            assert_eq!(loaded.problem, None);
        }
        let text = String::from_utf8(serialize(&Json, &Settings::default()).unwrap()).unwrap();
        assert_eq!(text, "{\n  \"version\": 1,\n  \"mode\": \"off\"\n}");
        let loaded = read(
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"https://dns.example/dns-query","bootstrap":["192.0.2.53"]}}"#,
        );
        assert!(matches!(loaded.settings.route(), Ok(Route::Proxy { .. })));
    }

    #[test]
    fn missing_broken_or_later_files_are_off_with_a_problem() {
        let missing = parse(&Json, Source::Missing);
        assert_eq!(missing.settings.mode, Mode::Off);
        assert!(!missing.present && missing.problem.is_none());
        assert_eq!(
            parse(&Json, Source::Unreadable).problem,
            Some(Problem::Unreadable)
        );
        let padded = format!(
            r#"{{"version":1,"mode":"direct","proxy":"{}"}}"#,
            "x".repeat(MAX_FILE_BYTES)
        );
        for (text, problem) in [
            (padded.as_str(), Problem::TooLarge),
            ("", Problem::Malformed),
            ("{", Problem::Malformed),
            ("[]", Problem::Malformed),
            (r#"{"mode":"direct"}"#, Problem::Malformed),
            (r#"{"version":"1","mode":"direct"}"#, Problem::Malformed),
            (r#"{"version":-1,"mode":"direct"}"#, Problem::Malformed),
            (
                r#"{"version":1,"mode":"direct","extra":true}"#,
                Problem::Malformed,
            ),
            (r#"{"version":1,"mode":"sometimes"}"#, Problem::Malformed),
            (r#"{"version":1}"#, Problem::Malformed),
            (
                r#"{"version":1,"mode":"direct","dns":{"url":"x","bootstrap":["nope"]}}"#,
                Problem::Malformed,
            ),
            (
                r#"{"version":2,"mode":"direct","new":"field"}"#,
                Problem::Version(2),
            ),
            (r#"{"version":0,"mode":"direct"}"#, Problem::Version(0)),
        ] {
            let loaded = read(text);
            assert_eq!(loaded.settings, Settings::default(), "{text}");
            assert_eq!(loaded.problem, Some(problem), "{text}");
            assert!(loaded.present);
            assert!(!loaded.may_overwrite());
        }
        // Problems never quote the file.
        let shown = Problem::Malformed.to_string();
        assert!(shown.ends_with("link previews are off") && !shown.contains("socks5"));
    }

    #[test]
    fn proxy_required_never_becomes_direct() {
        for text in [
            r#"{"version":1,"mode":"proxy_required"}"#,
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050"}"#,
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5h://127.0.0.1:9050","dns":{"url":"https://dns.example/","bootstrap":["192.0.2.53"]}}"#,
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"http://dns.example/","bootstrap":["192.0.2.53"]}}"#,
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"https://dns.example/","bootstrap":[]}}"#,
        ] {
            let loaded = read(text);
            assert_eq!(loaded.settings.mode, Mode::ProxyRequired, "{text}");
            assert!(
                matches!(loaded.problem, Some(Problem::Incomplete(_))),
                "{text}"
            );
            assert!(loaded.settings.route().is_err());
            assert!(loaded.may_overwrite());
        }
        // Saved incomplete settings read back the same way.
        let incomplete = Settings {
            mode: Mode::ProxyRequired,
            ..Settings::default()
        };
        let loaded = parse(
            &Json,
            Source::Bytes(&serialize(&Json, &incomplete).unwrap()),
        );
        assert_eq!(loaded.settings, incomplete);
        assert_eq!(
            loaded.problem,
            Some(Problem::Incomplete(PolicyError::MissingProxy))
        );
        // Settings too large for the file are refused, not cut.
        let huge = Settings {
            mode: Mode::ProxyRequired,
            proxy: Some("x".repeat(MAX_FILE_BYTES)),
            dns: None,
        };
        assert_eq!(serialize(&Json, &huge), Err(SaveError::TooLarge));
    }

    #[test]
    fn a_managed_sidecar_decides_alone_and_fails_closed() {
        let direct = read(r#"{"version":1,"mode":"direct"}"#);
        let absent = parse(&Json, Source::Missing);
        assert_eq!(effective(&direct, &absent), direct);
        let managed_off = read(r#"{"version":1,"mode":"off"}"#);
        assert_eq!(effective(&direct, &managed_off).settings.mode, Mode::Off);
        let managed_proxy = parse(
            &Json,
            Source::Bytes(&serialize(&Json, &proxy_settings()).unwrap()),
        );
        assert_eq!(
            effective(&direct, &managed_proxy).settings,
            proxy_settings()
        );
        // A managed file that cannot be used is Off, never the user's Direct.
        for broken in [
            parse(&Json, Source::Unreadable),
            read("{"),
            read(r#"{"version":9,"mode":"direct"}"#),
        ] {
            let effective = effective(&direct, &broken);
            assert_eq!(effective.settings.mode, Mode::Off);
            assert!(effective.problem.is_some());
        }
        // A managed ProxyRequired with incomplete settings stays that way.
        let incomplete = read(r#"{"version":1,"mode":"proxy_required"}"#);
        let effective = effective(&direct, &incomplete);
        assert_eq!(effective.settings.mode, Mode::ProxyRequired);
        assert!(effective.settings.route().is_err());
    }
}
