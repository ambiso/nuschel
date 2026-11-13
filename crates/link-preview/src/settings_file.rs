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
//! malformed or unknown-version file is Off with a [`Problem`] naming that
//! file, and the caller keeps the file as it is. A ProxyRequired file stays
//! ProxyRequired even when its proxy settings are incomplete or invalid (its
//! route then fails, so nothing is fetched): no reading ever yields Direct
//! unless the file says Direct. `managed-link-previews.json` (same format) can
//! only narrow the user's choice; see [`effective`]. The caller does the file
//! I/O.
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

/// Which sidecar a file or problem is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    User,
    Managed,
}

impl Origin {
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::User => FILE_NAME,
            Self::Managed => MANAGED_FILE_NAME,
        }
    }
}

/// Why the settings are not used as written. No text from a file is kept: a
/// parser message could quote a proxy address or resolver URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Unreadable(Origin),
    TooLarge(Origin),
    Malformed(Origin),
    /// Written by a different (normally later) version.
    Version(Origin, u32),
    /// The file names a proxy or resolver that is not valid.
    InvalidService(Origin, PolicyError),
    /// ProxyRequired without a complete, valid proxy and DNS configuration:
    /// nothing is fetched until it is fixed.
    Incomplete(PolicyError),
    /// A notice, not a failure: previews use the managed proxy or resolver
    /// instead of the user's own.
    ManagedServices,
}

impl Problem {
    /// Whether previews are off because of it (all but the notice).
    pub fn disables_previews(&self) -> bool {
        !matches!(self, Self::ManagedServices)
    }

    /// The file could not be used at all.
    fn unusable(&self) -> bool {
        matches!(
            self,
            Self::Unreadable(_) | Self::TooLarge(_) | Self::Malformed(_) | Self::Version(..)
        )
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(origin) => {
                write!(formatter, "{} could not be read", origin.file_name())
            }
            Self::TooLarge(origin) => write!(
                formatter,
                "{} exceeds {MAX_FILE_BYTES} bytes",
                origin.file_name()
            ),
            Self::Malformed(origin) => write!(formatter, "{} is not valid", origin.file_name()),
            Self::Version(origin, version) => write!(
                formatter,
                "{} has unknown version {version}",
                origin.file_name()
            ),
            Self::InvalidService(origin, error) => {
                write!(formatter, "{} is not valid: {error}", origin.file_name())
            }
            Self::Incomplete(error) => write!(formatter, "proxy settings incomplete: {error}"),
            Self::ManagedServices => {
                return write!(
                    formatter,
                    "link previews use the proxy settings from {MANAGED_FILE_NAME} instead of yours"
                );
            }
        }?;
        formatter.write_str("; link previews are off")
    }
}

/// The policy read from one sidecar, or in force from both.
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
        !self.problem.as_ref().is_some_and(Problem::unusable)
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

/// Read one sidecar; `origin` names it in any [`Problem`].
pub fn parse(origin: Origin, source: Source<'_>) -> Loaded {
    let bytes = match source {
        Source::Missing => return Loaded::off(false, None),
        Source::Unreadable => return Loaded::off(true, Some(Problem::Unreadable(origin))),
        Source::Bytes(bytes) => bytes,
    };
    if bytes.len() > MAX_FILE_BYTES {
        return Loaded::off(true, Some(Problem::TooLarge(origin)));
    }
    // The version first: a later version may have fields this one refuses.
    let Ok(probe) = serde_json::from_slice::<Probe>(bytes) else {
        return Loaded::off(true, Some(Problem::Malformed(origin)));
    };
    if probe.version != VERSION {
        return Loaded::off(true, Some(Problem::Version(origin, probe.version)));
    }
    let Ok(file) = serde_json::from_slice::<FileV1>(bytes) else {
        return Loaded::off(true, Some(Problem::Malformed(origin)));
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

/// The policy in force from the user's sidecar and the managed one, both
/// read with [`parse`]. A managed file that is unreadable, malformed, of an
/// unknown version, or names a proxy or resolver that is not valid means Off
/// with a [`Problem`] naming it: a broken administrator configuration never
/// falls back to the user's choice. A usable one only narrows the user's
/// mode (Off < ProxyRequired < Direct), and only one requiring a proxy
/// supplies proxy settings, with [`Problem::ManagedServices`] as a notice
/// when they replace the user's. ProxyRequired without a complete
/// configuration fetches nothing and says why. The caller passes only the
/// managed file from the administrator's locations (like `managed.json`).
pub fn effective(user: &Loaded, managed: &Loaded) -> Loaded {
    if !managed.present {
        return user.clone();
    }
    if managed.problem.as_ref().is_some_and(Problem::unusable) {
        return managed.clone();
    }
    if let Some(error) = invalid_services(&managed.settings) {
        return Loaded::off(true, Some(Problem::InvalidService(Origin::Managed, error)));
    }
    let (settings, replaced) = narrow(&user.settings, &managed.settings);
    let problem = match &user.problem {
        // The user's own file was not understood: it stays the notice.
        Some(problem) if problem.unusable() => Some(problem.clone()),
        _ => incomplete(&settings).or_else(|| {
            (replaced && settings.mode == Mode::ProxyRequired).then_some(Problem::ManagedServices)
        }),
    };
    Loaded {
        settings,
        present: true,
        problem,
    }
}

/// A proxy or resolver a managed file names must be valid even where its
/// mode does not use it.
fn invalid_services(settings: &Settings) -> Option<PolicyError> {
    if let Some(Err(error)) = settings.proxy.as_deref().map(Proxy::parse) {
        return Some(error);
    }
    if let Some(Err(error)) = settings.dns.as_ref().map(ProxiedDns::parse) {
        return Some(error);
    }
    None
}

/// The managed narrowing, kept apart so its rule is easy to change: the
/// narrower mode wins, and only a managed file requiring a proxy supplies
/// proxy settings (its own replace the user's). Also whether a user's own
/// proxy or resolver was replaced.
fn narrow(user: &Settings, managed: &Settings) -> (Settings, bool) {
    fn exposure(mode: Mode) -> u8 {
        match mode {
            Mode::Off => 0,
            Mode::ProxyRequired => 1,
            Mode::Direct => 2,
        }
    }
    let mode = if exposure(managed.mode) < exposure(user.mode) {
        managed.mode
    } else {
        user.mode
    };
    if managed.mode != Mode::ProxyRequired {
        return (
            Settings {
                mode,
                ..user.clone()
            },
            false,
        );
    }
    let replaced = (managed.proxy.is_some() && user.proxy.is_some() && managed.proxy != user.proxy)
        || (managed.dns.is_some() && user.dns.is_some() && managed.dns != user.dns);
    (
        Settings {
            mode,
            proxy: managed.proxy.clone().or_else(|| user.proxy.clone()),
            dns: managed.dns.clone().or_else(|| user.dns.clone()),
        },
        replaced,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Route;

    fn read(text: &str) -> Loaded {
        parse(Origin::User, Source::Bytes(text.as_bytes()))
    }

    fn managed(text: &str) -> Loaded {
        parse(Origin::Managed, Source::Bytes(text.as_bytes()))
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

    fn bytes(settings: &Settings) -> Vec<u8> {
        serialize(settings).unwrap()
    }

    #[test]
    fn every_mode_round_trips() {
        for settings in [mode(Mode::Off), mode(Mode::Direct), proxy_settings()] {
            let loaded = parse(Origin::User, Source::Bytes(&bytes(&settings)));
            assert_eq!(loaded.settings, settings);
            assert!(loaded.present);
            assert_eq!(loaded.problem, None);
        }
        let text = String::from_utf8(bytes(&Settings::default())).unwrap();
        assert_eq!(text, "{\n  \"version\": 1,\n  \"mode\": \"off\"\n}");
        let loaded = read(
            r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"https://dns.example/dns-query","bootstrap":["192.0.2.53"]}}"#,
        );
        assert!(matches!(loaded.settings.route(), Ok(Route::Proxy { .. })));
    }

    #[test]
    fn missing_broken_or_later_files_are_off_with_a_problem_naming_them() {
        let missing = parse(Origin::User, Source::Missing);
        assert_eq!(missing.settings.mode, Mode::Off);
        assert!(!missing.present && missing.problem.is_none());
        assert_eq!(
            parse(Origin::User, Source::Unreadable).problem,
            Some(Problem::Unreadable(Origin::User))
        );
        let padded = format!(
            r#"{{"version":1,"mode":"direct","proxy":"{}"}}"#,
            "x".repeat(MAX_FILE_BYTES)
        );
        let user = Origin::User;
        for (text, problem) in [
            (padded.as_str(), Problem::TooLarge(user)),
            ("", Problem::Malformed(user)),
            ("{", Problem::Malformed(user)),
            ("[]", Problem::Malformed(user)),
            (r#"{"mode":"direct"}"#, Problem::Malformed(user)),
            (
                r#"{"version":"1","mode":"direct"}"#,
                Problem::Malformed(user),
            ),
            (
                r#"{"version":-1,"mode":"direct"}"#,
                Problem::Malformed(user),
            ),
            (
                r#"{"version":1,"mode":"direct","extra":true}"#,
                Problem::Malformed(user),
            ),
            (
                r#"{"version":1,"mode":"sometimes"}"#,
                Problem::Malformed(user),
            ),
            (r#"{"version":1}"#, Problem::Malformed(user)),
            (
                r#"{"version":1,"mode":"direct","dns":{"url":"x","bootstrap":["nope"]}}"#,
                Problem::Malformed(user),
            ),
            (
                r#"{"version":2,"mode":"direct","new":"field"}"#,
                Problem::Version(user, 2),
            ),
            (
                r#"{"version":0,"mode":"direct"}"#,
                Problem::Version(user, 0),
            ),
        ] {
            let loaded = read(text);
            assert_eq!(loaded.settings, Settings::default(), "{text}");
            assert_eq!(loaded.problem, Some(problem), "{text}");
            assert!(loaded.present);
            assert!(!loaded.may_overwrite());
        }
        // Problems name the file and never quote it.
        let shown = Problem::Malformed(Origin::User).to_string();
        assert_eq!(
            shown,
            "link-previews.json is not valid; link previews are off"
        );
        assert!(
            Problem::TooLarge(Origin::Managed)
                .to_string()
                .starts_with("managed-link-previews.json exceeds")
        );
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
        let loaded = parse(Origin::User, Source::Bytes(&bytes(&incomplete)));
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
        let user_proxy = parse(Origin::User, Source::Bytes(&bytes(&proxy_settings())));
        let absent = parse(Origin::Managed, Source::Missing);
        assert_eq!(effective(&user_direct, &absent), user_direct);
        // Every (user, managed) pair: the narrower mode wins.
        let users = [&user_off, &user_proxy, &user_direct];
        let caps = [Mode::Off, Mode::ProxyRequired, Mode::Direct];
        for (u, user) in users.iter().enumerate() {
            for (c, cap) in caps.iter().enumerate() {
                let managed = parse(Origin::Managed, Source::Bytes(&bytes(&mode(*cap))));
                let effective = effective(user, &managed);
                assert_eq!(
                    effective.settings.mode,
                    [caps[u], caps[c]][usize::from(c < u)]
                );
            }
        }
        // The user's Off stays Off whatever the managed file says.
        let managed_direct = managed(r#"{"version":1,"mode":"direct"}"#);
        assert_eq!(
            effective(&user_off, &managed_direct).settings.mode,
            Mode::Off
        );
        let user_missing = parse(Origin::User, Source::Missing);
        assert_eq!(
            effective(&user_missing, &managed_direct).settings.mode,
            Mode::Off
        );
        // A user file that was not understood stays Off and keeps its notice.
        let effective_broken = effective(&read("{"), &managed_direct);
        assert_eq!(effective_broken.settings.mode, Mode::Off);
        assert_eq!(
            effective_broken.problem,
            Some(Problem::Malformed(Origin::User))
        );
    }

    #[test]
    fn a_managed_proxy_requirement_without_a_proxy_fetches_nothing() {
        let user_direct = read(r#"{"version":1,"mode":"direct"}"#);
        let managed_proxy_only = managed(r#"{"version":1,"mode":"proxy_required"}"#);
        let effective = effective(&user_direct, &managed_proxy_only);
        assert_eq!(effective.settings.mode, Mode::ProxyRequired);
        assert!(effective.settings.route().is_err());
        assert_eq!(
            effective.problem,
            Some(Problem::Incomplete(PolicyError::MissingProxy))
        );
        assert!(effective.problem.unwrap().disables_previews());
        // With the user's own complete proxy settings it works as they are.
        let user_proxy = parse(Origin::User, Source::Bytes(&bytes(&proxy_settings())));
        let narrowed = super::effective(&user_proxy, &managed_proxy_only);
        assert_eq!(narrowed.settings, proxy_settings());
        assert_eq!(narrowed.problem, None);
    }

    #[test]
    fn only_a_managed_proxy_requirement_supplies_proxy_settings() {
        let user_proxy = parse(Origin::User, Source::Bytes(&bytes(&proxy_settings())));
        let user_direct = read(r#"{"version":1,"mode":"direct"}"#);
        let admin = r#""proxy":"socks5://10.0.0.2:1080","dns":{"url":"https://resolver.example/dns-query","bootstrap":["10.0.0.53"]}"#;
        // A Direct managed file does not replace the user's Tor proxy.
        let managed_direct = managed(&format!(r#"{{"version":1,"mode":"direct",{admin}}}"#));
        let kept = effective(&user_proxy, &managed_direct);
        assert_eq!(kept.settings, proxy_settings());
        assert_eq!(kept.problem, None);
        // A ProxyRequired managed file replaces it, with a notice.
        let managed_proxy = managed(&format!(
            r#"{{"version":1,"mode":"proxy_required",{admin}}}"#
        ));
        let replaced = effective(&user_proxy, &managed_proxy);
        let Ok(Route::Proxy { proxy, dns }) = replaced.settings.route() else {
            panic!("{replaced:?}");
        };
        assert_eq!(proxy.address(), "10.0.0.2:1080".parse().unwrap());
        assert_eq!(dns.url().as_str(), "https://resolver.example/dns-query");
        assert_eq!(replaced.problem, Some(Problem::ManagedServices));
        assert!(!Problem::ManagedServices.disables_previews());
        assert!(
            Problem::ManagedServices
                .to_string()
                .contains("managed-link-previews.json instead of yours")
        );
        // Without proxy settings of the user's own there is nothing to replace.
        let supplied = effective(&user_direct, &managed_proxy);
        assert_eq!(supplied.settings.mode, Mode::ProxyRequired);
        assert!(supplied.settings.route().is_ok());
        assert_eq!(supplied.problem, None);
    }

    #[test]
    fn an_unusable_or_invalid_managed_file_is_off_and_named() {
        let user_direct = read(r#"{"version":1,"mode":"direct"}"#);
        let oversized = vec![b' '; MAX_FILE_BYTES + 1];
        for (broken, problem) in [
            (
                parse(Origin::Managed, Source::Unreadable),
                Problem::Unreadable(Origin::Managed),
            ),
            (
                parse(Origin::Managed, Source::Bytes(&oversized)),
                Problem::TooLarge(Origin::Managed),
            ),
            (managed("{"), Problem::Malformed(Origin::Managed)),
            (
                managed(r#"{"version":9,"mode":"direct"}"#),
                Problem::Version(Origin::Managed, 9),
            ),
        ] {
            let effective = effective(&user_direct, &broken);
            assert_eq!(effective.settings, Settings::default());
            assert!(
                effective
                    .problem
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .starts_with(MANAGED_FILE_NAME)
            );
            assert_eq!(effective.problem, Some(problem));
        }
        // A managed file naming an invalid proxy or resolver is Off, even
        // where its mode does not use them, and says so.
        for (text, error) in [
            (
                r#"{"version":1,"mode":"direct","proxy":"socks5h://127.0.0.1:9050"}"#,
                PolicyError::Proxy,
            ),
            (
                r#"{"version":1,"mode":"proxy_required","proxy":"socks5://127.0.0.1:9050","dns":{"url":"http://dns.example/","bootstrap":["192.0.2.53"]}}"#,
                PolicyError::Dns,
            ),
            (
                r#"{"version":1,"mode":"proxy_required","proxy":"socks5://proxy.example:1080"}"#,
                PolicyError::Proxy,
            ),
        ] {
            let effective = effective(&user_direct, &managed(text));
            assert_eq!(effective.settings, Settings::default(), "{text}");
            assert_eq!(
                effective.problem,
                Some(Problem::InvalidService(Origin::Managed, error)),
                "{text}"
            );
            assert!(
                effective
                    .problem
                    .unwrap()
                    .to_string()
                    .starts_with("managed-link-previews.json is not valid: ")
            );
        }
    }
}
