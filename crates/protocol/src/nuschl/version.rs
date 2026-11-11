// SPDX-License-Identifier: EUPL-1.2
//! Client versions for the minimum-version policy (docs/extension.md,
//! "Minimum versions"): the client reports its own version in the hello, the
//! server announces `minimum_client` and `recommended_client` and refuses
//! clients below the minimum from an announced date on. Self-reported and
//! advisory: this keeps the ecosystem current, it is not a security control.
use super::v1;
pub use semver::Version;
use std::cmp::Ordering;

/// `Hello.client_version` is at most this long; longer fails the hello.
pub const MAX_CLIENT_VERSION_BYTES: usize = 64;

/// A policy version in Mumble's 1.5 format (`major << 48 | minor << 32 |
/// patch << 16`) as semver; `None` for 0, which means "none".
pub fn from_wire(value: u64) -> Option<Version> {
    (value != 0).then(|| Version::new(value >> 48, (value >> 32) & 0xffff, (value >> 16) & 0xffff))
}

/// A release version in Mumble's 1.5 format; `None` if a part exceeds 16 bits.
pub fn to_wire(version: &Version) -> Option<u64> {
    let part = |value: u64| u16::try_from(value).ok().map(u64::from);
    Some(part(version.major)? << 48 | part(version.minor)? << 32 | part(version.patch)? << 16)
}

/// A client's self-reported version; `None` when missing or not semver.
pub fn parse(text: &str) -> Option<Version> {
    (text.len() <= MAX_CLIENT_VERSION_BYTES)
        .then(|| Version::parse(text).ok())
        .flatten()
}

/// Whether `client` is older than `required` (1.5 format, 0 for none) by
/// semver precedence: a pre-release is older than its release, build metadata
/// is ignored, and an unknown version is older than any requirement.
pub fn below(client: Option<&Version>, required: u64) -> bool {
    from_wire(required).is_some_and(|required| {
        client.is_none_or(|client| client.cmp_precedence(&required) == Ordering::Less)
    })
}

/// Where a client stands with a server's policy at `now` (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    /// At or above the recommended version (or the server names none).
    Current,
    /// Below `recommended_client`: an update is suggested.
    Recommended { version: Version },
    /// Below `minimum_client`, admitted until `from` (Unix seconds).
    Required { version: Version, from: i64 },
    /// Below `minimum_client` from its date on: the server refuses it.
    Refused { version: Version },
}

impl Standing {
    pub fn of(policy: &v1::Policy, client: Option<&Version>, now: i64) -> Self {
        if below(client, policy.minimum_client) {
            let version = from_wire(policy.minimum_client).unwrap_or_else(|| Version::new(0, 0, 0));
            return if now < policy.minimum_client_from {
                Self::Required {
                    version,
                    from: policy.minimum_client_from,
                }
            } else {
                Self::Refused { version }
            };
        }
        match from_wire(policy.recommended_client) {
            Some(version) if below(client, policy.recommended_client) => {
                Self::Recommended { version }
            }
            _ => Self::Current,
        }
    }
}

/// The reason a refused client is shown (in a Mumble `Reject`).
pub fn refusal(minimum: &Version) -> String {
    format!("This server requires Nuschl {minimum} or newer. Update Nuschl to connect.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(text: &str) -> u64 {
        to_wire(&Version::parse(text).unwrap()).unwrap()
    }

    #[test]
    fn versions_compare_by_semver_precedence() {
        let version = |text| parse(text);
        assert_eq!(wire("1.5.0"), 0x0001_0005_0000_0000);
        assert_eq!(from_wire(wire("0.10.2")), Some(Version::new(0, 10, 2)));
        assert_eq!(from_wire(0), None);
        // Numeric, not textual: 0.10 is newer than 0.9.
        assert!(below(version("0.9.9").as_ref(), wire("0.10.0")));
        assert!(!below(version("0.10.0").as_ref(), wire("0.9.0")));
        assert!(!below(version("0.10.0").as_ref(), wire("0.10.0")));
        // A pre-release is older than its release; build metadata is ignored.
        assert!(below(version("0.3.0-rc.1").as_ref(), wire("0.3.0")));
        assert!(!below(version("0.3.0+abc").as_ref(), wire("0.3.0")));
        assert!(!below(version("0.3.1-rc.1").as_ref(), wire("0.3.0")));
        // Unknown is older than any requirement; no requirement holds back nobody.
        assert!(below(None, wire("0.0.1")));
        assert!(!below(None, 0));
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse(&format!("1.0.0-{}", "a".repeat(64))), None);
        assert_eq!(to_wire(&Version::new(70_000, 0, 0)), None);
    }

    #[test]
    fn standing_follows_the_policy_and_its_date() {
        let policy = v1::Policy {
            minimum_client: wire("0.3.0"),
            recommended_client: wire("0.5.0"),
            minimum_client_from: 1_000,
            ..Default::default()
        };
        let at = |text: &str, now| Standing::of(&policy, parse(text).as_ref(), now);
        assert_eq!(at("0.5.0", 0), Standing::Current);
        assert_eq!(
            at("0.4.9", 0),
            Standing::Recommended {
                version: Version::new(0, 5, 0)
            }
        );
        assert_eq!(
            at("0.2.0", 999),
            Standing::Required {
                version: Version::new(0, 3, 0),
                from: 1_000
            }
        );
        assert_eq!(
            at("0.2.0", 1_000),
            Standing::Refused {
                version: Version::new(0, 3, 0)
            }
        );
        // Without a date the minimum applies at once.
        let now = v1::Policy {
            minimum_client_from: 0,
            ..policy
        };
        assert_eq!(
            Standing::of(&now, None, 5),
            Standing::Refused {
                version: Version::new(0, 3, 0)
            }
        );
        assert_eq!(
            Standing::of(&v1::Policy::default(), None, 5),
            Standing::Current
        );
    }
}
