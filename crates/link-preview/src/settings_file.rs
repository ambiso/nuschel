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
//! unless the file says Direct. `managed-link-previews.json` (same format) can
//! only narrow the user's choice; see [`cap`]. The caller does the file I/O.
use crate::policy::{DnsSettings, Error as PolicyError, Mode, ProxiedDns, Proxy, Settings};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "link-previews.json";
pub const MANAGED_FILE_NAME: &str = "managed-link-previews.json";
pub const VERSION: u32 = 1;
/// Larger files are not parsed.
pub const MAX_FILE_BYTES: usize = 4096;

/// What reading the file gave.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    Missing,
    /// The file exists but could not be read (permissions, I/O, or longer
    /// than [`MAX_FILE_BYTES`] when the caller stopped reading early).
    Unreadable,
    Bytes(&'a [u8]),
}

/// Why a file was not used as written. No text from the file is kept: a
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
            Self::Unreadable => formatter.write_str("the link preview settings could not be read"),
            Self::TooLarge => write!(
                formatter,
                "the link preview settings exceed {MAX_FILE_BYTES} bytes"
            ),
            Self::Malformed => formatter.write_str("the link preview settings are not valid"),
            Self::Version(version) => write!(
                formatter,
                "the link preview settings have unknown version {version}"
            ),
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
    dns: Option<DnsSettings>,
}

/// Read one sidecar (the user's or the managed one).
pub fn parse(source: Source<'_>) -> Loaded {
    let bytes = match source {
        Source::Missing => return Loaded::off(false, None),
        Source::Unreadable => return Loaded::off(true, Some(Problem::Unreadable)),
        Source::Bytes(bytes) => bytes,
    };
    if bytes.len() > MAX_FILE_BYTES {
        return Loaded::off(true, Some(Problem::TooLarge));
    }
    // The version first: a later version may have fields this one refuses.
    let Ok(probe) = serde_json::from_slice::<Probe>(bytes) else {
        return Loaded::off(true, Some(Problem::Malformed));
    };
    if probe.version != VERSION {
        return Loaded::off(true, Some(Problem::Version(probe.version)));
    }
    let Ok(file) = serde_json::from_slice::<FileV1>(bytes) else {
        return Loaded::off(true, Some(Problem::Malformed));
    };
    let settings = Settings {
        mode: file.mode,
        proxy: file.proxy,
        dns: file.dns,
    };
    Loaded {
        problem: incomplete(&settings),
        settings,
        present: true,
    }
}

fn incomplete(settings: &Settings) -> Option<Problem> {
    match (settings.mode, settings.route()) {
        (Mode::ProxyRequired, Err(error)) => Some(Problem::Incomplete(error)),
        _ => None,
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
pub fn serialize(settings: &Settings) -> Result<Vec<u8>, SaveError> {
    let file = FileV1 {
        version: VERSION,
        mode: settings.mode,
        proxy: settings.proxy.clone(),
        dns: settings.dns.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|_| SaveError::Encode)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(SaveError::TooLarge);
    }
    Ok(bytes)
}

/// The policy in force from the user's sidecar and the managed one. A
/// managed file that is unreadable, malformed, of an unknown version, or
/// names a proxy or resolver that is not valid means Off with its
/// [`Problem`]: a broken administrator configuration never falls back to the
/// user's choice. A usable one narrows the user's settings with [`cap`]; when
/// the result is ProxyRequired without a complete configuration, nothing is
/// fetched and the problem says why. The caller passes only the managed file
/// from the administrator's locations (like `managed.json`).
pub fn effective(user: &Loaded, managed: &Loaded) -> Loaded {
    if !managed.present {
        return user.clone();
    }
    if matches!(
        managed.problem,
        Some(Problem::Unreadable | Problem::TooLarge | Problem::Malformed | Problem::Version(_))
    ) {
        return managed.clone();
    }
    if let Some(error) = invalid_services(&managed.settings) {
        return Loaded::off(true, Some(Problem::Incomplete(error)));
    }
    let settings = cap(&user.settings, &managed.settings);
    let problem = match &user.problem {
        // The user's own file was not understood: it stays the notice.
        Some(Problem::Incomplete(_)) | None => incomplete(&settings),
        Some(problem) => Some(problem.clone()),
    };
    Loaded {
        settings,
        present: true,
        problem,
    }
}

/// A proxy or resolver a file names must be valid even where its mode does
/// not use it.
fn invalid_services(settings: &Settings) -> Option<PolicyError> {
    if let Some(Err(error)) = settings.proxy.as_deref().map(Proxy::parse) {
        return Some(error);
    }
    if let Some(Err(error)) = settings.dns.as_ref().map(ProxiedDns::parse) {
        return Some(error);
    }
    None
}

/// The managed cap, kept apart so its semantics are easy to change: the
/// mode is the narrower of the two (Off < ProxyRequired < Direct), so a
/// managed file never turns Off into anything else nor ProxyRequired into
/// Direct. A proxy or resolver the managed file names replaces the user's.
pub fn cap(user: &Settings, managed: &Settings) -> Settings {
    fn exposure(mode: Mode) -> u8 {
        match mode {
            Mode::Off => 0,
            Mode::ProxyRequired => 1,
            Mode::Direct => 2,
        }
    }
    Settings {
        mode: if exposure(managed.mode) < exposure(user.mode) {
            managed.mode
        } else {
            user.mode
        },
        proxy: managed.proxy.clone().or_else(|| user.proxy.clone()),
        dns: managed.dns.clone().or_else(|| user.dns.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Route;

    fn read(text: &str) -> Loaded {
        parse(Source::Bytes(text.as_bytes()))
    }

    fn dns() -> DnsSettings {
        DnsSettings {
            url: "https://dns.example/dns-query".into(),
            bootstrap: vec![
                "192.0.2.53".parse().unwrap(),
                "2001:db8::53".parse().unwrap(),
            ],
        }
    }

    fn proxy_settings() -> Settings {
        Settings {
            mode: Mode::ProxyRequired,
            proxy: Some("socks5://127.0.0.1:9050".into()),
            dns: Some(dns()),
        }
    }

    fn mode(mode: Mode) -> Settings {
        Settings {
            mode,
            ..Settings::default()
        }
    }

    #[test]
    fn every_mode_round_trips() {
        for settings in [mode(Mode::Off), mode(Mode::Direct), proxy_settings()] {
            let bytes = serialize(&settings).unwrap();
            let loaded = parse(Source::Bytes(&bytes));
            assert_eq!(loaded.settings, settings);
            assert!(loaded.present);
            assert_eq!(loaded.problem, None);
        }
        let text = String::from_utf8(serialize(&Settings::default()).unwrap()).unwrap();
        assert_eq!(text, "{\n  \"version\": 1,\n  \"mode\": \"off\"\n}");
        let loaded = read(
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"https://dns.example/dns-query","bootstrap":["192.0.2.53"]}}"#,
        );
        assert!(matches!(loaded.settings.route(), Ok(Route::Proxy { .. })));
    }

    #[test]
    fn missing_broken_or_later_files_are_off_with_a_problem() {
        let missing = parse(Source::Missing);
        assert_eq!(missing.settings.mode, Mode::Off);
        assert!(!missing.present && missing.problem.is_none());
        assert_eq!(parse(Source::Unreadable).problem, Some(Problem::Unreadable));
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
        let incomplete = mode(Mode::ProxyRequired);
        let loaded = parse(Source::Bytes(&serialize(&incomplete).unwrap()));
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
        assert_eq!(serialize(&huge), Err(SaveError::TooLarge));
    }

    #[test]
    fn a_managed_file_only_narrows_the_user_mode() {
        let user_off = read(r#"{"version":1,"mode":"off"}"#);
        let user_direct = read(r#"{"version":1,"mode":"direct"}"#);
        let user_proxy = parse(Source::Bytes(&serialize(&proxy_settings()).unwrap()));
        let absent = parse(Source::Missing);
        assert_eq!(effective(&user_direct, &absent), user_direct);
        // Every (user, managed) pair: the narrower mode wins.
        let users = [&user_off, &user_proxy, &user_direct];
        let caps = [Mode::Off, Mode::ProxyRequired, Mode::Direct];
        for (u, user) in users.iter().enumerate() {
            for (c, cap_mode) in caps.iter().enumerate() {
                let managed = parse(Source::Bytes(&serialize(&mode(*cap_mode)).unwrap()));
                let effective = effective(user, &managed);
                assert_eq!(
                    effective.settings.mode,
                    [caps[u], caps[c]][usize::from(c < u)]
                );
            }
        }
        // Off stays Off whatever the managed file says.
        let managed_direct = read(r#"{"version":1,"mode":"direct"}"#);
        assert_eq!(
            effective(&user_off, &managed_direct).settings.mode,
            Mode::Off
        );
        assert_eq!(effective(&absent, &managed_direct).settings.mode, Mode::Off);
        // A managed cap at ProxyRequired keeps the user's proxy, or replaces
        // it with its own; without any proxy nothing is fetched.
        let managed_proxy_only = read(r#"{"version":1,"mode":"proxy_required"}"#);
        let narrowed = effective(&user_proxy, &managed_proxy_only);
        assert!(matches!(narrowed.settings.route(), Ok(Route::Proxy { .. })));
        assert_eq!(narrowed.problem, None);
        let from_direct = effective(&user_direct, &managed_proxy_only);
        assert_eq!(from_direct.settings.mode, Mode::ProxyRequired);
        assert!(from_direct.settings.route().is_err());
        assert!(matches!(from_direct.problem, Some(Problem::Incomplete(_))));
        let managed_with_proxy = parse(Source::Bytes(&serialize(&proxy_settings()).unwrap()));
        let supplied = effective(&user_direct, &managed_with_proxy);
        assert_eq!(supplied.settings, proxy_settings());
        assert_eq!(supplied.problem, None);
        // The managed proxy replaces the user's own.
        let managed_other_proxy = read(
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://10.0.0.2:1080","dns":{"url":"https://resolver.example/dns-query","bootstrap":["10.0.0.53"]}}"#,
        );
        let replaced = effective(&user_proxy, &managed_other_proxy);
        let Ok(Route::Proxy { proxy, dns }) = replaced.settings.route() else {
            panic!("{replaced:?}");
        };
        assert_eq!(proxy.address(), "10.0.0.2:1080".parse().unwrap());
        assert_eq!(dns.url().as_str(), "https://resolver.example/dns-query");
        assert_eq!(replaced.problem, None);
        // A managed file that cannot be used is Off, never the user's Direct.
        for broken in [
            parse(Source::Unreadable),
            read("{"),
            read(r#"{"version":9,"mode":"direct"}"#),
        ] {
            let effective = effective(&user_direct, &broken);
            assert_eq!(effective.settings.mode, Mode::Off);
            assert!(effective.problem.is_some());
        }
        // A managed file naming an invalid proxy or resolver is Off, even
        // where its mode does not use them.
        for broken in [
            r#"{"version":1,"mode":"direct","proxy":"socks5h://127.0.0.1:9050"}"#,
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"http://dns.example/","bootstrap":["192.0.2.53"]}}"#,
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://proxy.example:1080"}"#,
        ] {
            let effective = effective(&user_proxy, &read(broken));
            assert_eq!(effective.settings, Settings::default(), "{broken}");
            assert!(
                matches!(effective.problem, Some(Problem::Incomplete(_))),
                "{broken}"
            );
        }
        // A user file that was not understood stays Off and keeps its notice.
        let user_broken = read("{");
        let effective = effective(&user_broken, &managed_direct);
        assert_eq!(effective.settings.mode, Mode::Off);
        assert_eq!(effective.problem, Some(Problem::Malformed));
    }
}
