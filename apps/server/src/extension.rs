// SPDX-License-Identifier: EUPL-1.2
//! The Nuschl extension's negotiation (docs/extension.md §1): it is on if and
//! only if ALPN selected `nuschl/1` and both hellos are present; the client's
//! hello is accepted once, before `Authenticate`, and the capability set is
//! then frozen for the connection. Anything else fails closed.
use crate::{
    config::{Config, GuestHistory, Settings},
    limits::{ByteBucket, LeakyBucket},
};
use nuschl_protocol::{
    control as proto,
    nuschl::{self, Id, capability, v1, version},
};
use std::{
    collections::{BTreeSet, HashMap},
    time::Instant,
};

/// A set of the capabilities this server implements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities(u32);

impl Capabilities {
    fn bit(name: &str) -> u32 {
        capability::ALL
            .iter()
            .position(|known| *known == name)
            .map_or(0, |index| 1 << index)
    }

    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self(
            names
                .into_iter()
                .fold(0, |bits, name| bits | Self::bit(name)),
        )
    }

    pub fn has(self, name: &str) -> bool {
        let bit = Self::bit(name);
        bit != 0 && self.0 & bit != 0
    }

    pub fn names(self) -> Vec<String> {
        capability::ALL
            .iter()
            .filter(|name| self.has(name))
            .map(|name| (*name).to_owned())
            .collect()
    }
}

/// ALPN selected `nuschl/1` but the client sent no hello before `Authenticate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissingHello;

/// What a connection has negotiated before authenticating.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Negotiation {
    /// No `nuschl/1` ALPN: a stock connection; field 7200 is ignored.
    #[default]
    Stock,
    /// ALPN selected `nuschl/1`, but the client's hello has not arrived.
    Offered,
    /// Both hellos are present; frozen.
    Agreed {
        capabilities: Capabilities,
        /// The version the client reports (semver), if it is one.
        client: Option<version::Version>,
        relay_only: bool,
    },
}

impl Negotiation {
    pub fn from_alpn(alpn: Option<&[u8]>) -> Self {
        if alpn == Some(nuschl::ALPN) {
            Self::Offered
        } else {
            Self::Stock
        }
    }

    /// Apply a `Version` received before authentication. Only the first hello
    /// counts; later ones cannot change the capabilities. `Err` closes the
    /// connection (a malformed hello on a `nuschl/1` connection).
    pub fn version(&mut self, body: &[u8], server: Capabilities) -> Result<(), nuschl::HelloError> {
        if *self != Self::Offered {
            return Ok(());
        }
        if let Some(hello) = nuschl::hello(body)? {
            let theirs = Capabilities::from_names(hello.capabilities.iter().map(String::as_str));
            *self = Self::Agreed {
                capabilities: Capabilities(theirs.0 & server.0),
                client: version::parse(&hello.client_version),
                relay_only: server.has(capability::PRIVACY)
                    && theirs.has(capability::PRIVACY)
                    && hello.relay_only,
            };
        }
        Ok(())
    }

    pub fn relay_only(&self) -> bool {
        matches!(
            self,
            Self::Agreed {
                relay_only: true,
                ..
            }
        )
    }

    /// At `Authenticate`: the extension's capabilities, `Ok(None)` for a stock
    /// connection, or `Err` when ALPN said `nuschl/1` but no hello came.
    pub fn freeze(&self) -> Result<Option<Capabilities>, MissingHello> {
        match self {
            Self::Stock => Ok(None),
            Self::Offered => Err(MissingHello),
            Self::Agreed { capabilities, .. } => Ok(Some(*capabilities)),
        }
    }

    /// At `Authenticate`: why a Nuschl client is refused under the
    /// minimum-version policy at `now` (Unix seconds), or because it cannot take
    /// part in an end-to-end server (it did not agree on `e2ee/1`), if it is.
    /// Stock connections never are: the security mode decides about them.
    pub fn refusal(&self, settings: &Settings, now: i64) -> Option<String> {
        let Self::Agreed {
            client,
            capabilities,
            ..
        } = self
        else {
            return None;
        };
        if settings.config.security == crate::config::Security::E2ee
            && !capabilities.has(capability::E2EE)
        {
            return Some(
                "This server encrypts every conversation end to end. Update Nuschl to \
                 connect."
                    .into(),
            );
        }
        let minimum = settings.minimum_client?;
        (now >= settings.minimum_client_from.unwrap_or(0)
            && version::below(client.as_ref(), minimum))
        .then_some(minimum)
        .and_then(version::from_wire)
        .map(|minimum| version::refusal(&minimum))
    }
}

/// A connection's extension state: what it negotiated, its requests in
/// flight, subscriptions and the extension's own rate limits.
pub struct Extension {
    pub capabilities: Capabilities,
    /// Never send this user's address to another connected client.
    pub relay_only: bool,
    /// Request ids in flight; at most [`nuschl::MAX_OUTSTANDING`].
    pub outstanding: Vec<u64>,
    pub subscriptions: BTreeSet<Id>,
    /// History pages, and their bytes.
    pub pages: LeakyBucket,
    pub page_bytes: ByteBucket,
    /// Subscribe, list and settings requests.
    pub requests: LeakyBucket,
    pub markers: LeakyBucket,
    /// When this connection last typed in each conversation.
    pub typing: HashMap<Id, Instant>,
    /// Key-transparency lookups; histories, audits and heads; publications.
    pub kt_lookups: LeakyBucket,
    pub kt_histories: LeakyBucket,
    pub kt_publishes: LeakyBucket,
}

impl Extension {
    pub fn new(capabilities: Capabilities, config: &Config, now: Instant) -> Self {
        let history = &config.history;
        let per_minute = history.pages_per_minute.max(1);
        Self {
            capabilities,
            relay_only: false,
            outstanding: Vec::new(),
            subscriptions: BTreeSet::new(),
            pages: LeakyBucket::new(per_minute.div_ceil(60), per_minute.div_ceil(4), now),
            page_bytes: ByteBucket::new(u64::from(history.page_bytes_per_minute), now),
            requests: LeakyBucket::new(5, 30, now),
            markers: LeakyBucket::new(5, 30, now),
            typing: HashMap::new(),
            kt_lookups: LeakyBucket::per_minute(
                config.kt.lookups_per_minute,
                config.kt.lookups_per_minute.div_ceil(4).max(8),
                now,
            ),
            kt_histories: LeakyBucket::per_minute(
                config.kt.histories_per_minute,
                config.kt.histories_per_minute.div_ceil(4).max(8),
                now,
            ),
            kt_publishes: LeakyBucket::per_minute(config.kt.publishes_per_minute, 3, now),
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.capabilities.has(name)
    }

    /// Register a request id; `Err` with the code to answer otherwise.
    pub fn begin(&mut self, request_id: u64) -> Result<(), v1::error::Code> {
        if self.outstanding.contains(&request_id) {
            return Err(v1::error::Code::DuplicateRequest);
        }
        if self.outstanding.len() >= nuschl::MAX_OUTSTANDING {
            return Err(v1::error::Code::TooManyRequests);
        }
        self.outstanding.push(request_id);
        Ok(())
    }

    pub fn end(&mut self, request_id: u64) {
        self.outstanding.retain(|id| *id != request_id);
    }
}

/// What this server offers: everything, `kt/1` only with a key-transparency log,
/// and `mls/1` only with `kt/1` and the delivery service (credentials are checked
/// against key transparency).
///
/// In end-to-end mode `typing/1` is not offered: typing indicators are protected
/// content (docs/e2ee.md §2) and travel only inside MLS, which is not built yet.
pub fn server_capabilities(kt: bool, mls: bool, e2ee: bool) -> Capabilities {
    Capabilities::from_names(
        capability::ALL
            .into_iter()
            .filter(|name| kt || (*name != capability::KT && *name != capability::KT_WITNESS))
            .filter(|name| *name != capability::MLS || (kt && mls))
            .filter(|name| *name != capability::E2EE || (kt && mls && e2ee))
            .filter(|name| !e2ee || *name != capability::TYPING),
    )
}

/// The server's hello, sent in its `Version` to `nuschl/1` connections, with
/// the descriptor of its key-transparency log if it has one.
pub fn server_hello(settings: &Settings, kt_log: Option<&[u8]>, mls: bool) -> v1::Hello {
    let history = &settings.config.history;
    let e2ee = settings.config.security == crate::config::Security::E2ee;
    let (retention_days, live_only) = crate::retention::wire(history.default_retention());
    v1::Hello {
        protocol: nuschl::PROTOCOL,
        capabilities: server_capabilities(kt_log.is_some(), mls, e2ee).names(),
        kt_log: kt_log.map(<[u8]>::to_vec).unwrap_or_default(),
        client_version: String::new(),
        relay_only: false,
        policy: Some(v1::Policy {
            mode: if e2ee {
                v1::policy::Mode::E2ee
            } else {
                v1::policy::Mode::Compatible
            } as i32,
            minimum_client: settings.minimum_client.unwrap_or(0),
            recommended_client: settings.recommended_client.unwrap_or(0),
            retention_days,
            guests: match history.guests {
                GuestHistory::None => v1::policy::Guests::None,
                GuestHistory::Full => v1::policy::Guests::Full,
            } as i32,
            edit_limit: history.edit_limit,
            moderators_delete: history.moderators_delete,
            live_only,
            minimum_client_from: settings
                .minimum_client
                .and(settings.minimum_client_from)
                .unwrap_or(0),
        }),
    }
}

/// The server's `Version`, with or without the hello.
pub fn version_frame(version: &proto::Version, hello: Option<&v1::Hello>) -> bytes::Bytes {
    nuschl::version_frame(version, hello).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_protocol::HEADER_BYTES;

    fn hello_body(capabilities: &[&str]) -> Vec<u8> {
        hello_body_from(capabilities, "")
    }

    fn hello_body_from(capabilities: &[&str], client_version: &str) -> Vec<u8> {
        let hello = v1::Hello {
            protocol: nuschl::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
            client_version: client_version.into(),
            relay_only: false,
        };
        nuschl::version_frame(&proto::Version::default(), Some(&hello)).unwrap()[HEADER_BYTES..]
            .to_vec()
    }

    #[test]
    fn the_extension_needs_alpn_and_a_hello() {
        let server = server_capabilities(true, true, false);
        // ALPN without a hello fails closed.
        assert_eq!(
            Negotiation::from_alpn(Some(nuschl::ALPN)).freeze(),
            Err(MissingHello)
        );
        // A hello without ALPN is ignored: a stock connection.
        let mut stock = Negotiation::from_alpn(None);
        stock.version(&hello_body(&["history/1"]), server).unwrap();
        assert_eq!(stock.freeze(), Ok(None));
        let mut mumble = Negotiation::from_alpn(Some(nuschl::ALPN_MUMBLE));
        mumble.version(&hello_body(&["history/1"]), server).unwrap();
        assert_eq!(mumble.freeze(), Ok(None));
        // Both: the intersection, frozen at the first hello.
        let mut agreed = Negotiation::from_alpn(Some(nuschl::ALPN));
        agreed
            .version(&hello_body(&["history/1", "unknown/3"]), server)
            .unwrap();
        agreed
            .version(
                &hello_body(&["history/1", "typing/1", "subscribe/1"]),
                server,
            )
            .unwrap();
        let frozen = agreed.freeze().unwrap().unwrap();
        assert!(frozen.has("history/1"));
        assert!(!frozen.has("typing/1"), "later hellos change nothing");
        // A plain Version on a nuschl/1 connection leaves it waiting.
        let mut waiting = Negotiation::from_alpn(Some(nuschl::ALPN));
        waiting
            .version(&proto::Version::default().encode_to_vec(), server)
            .unwrap();
        assert_eq!(waiting.freeze(), Err(MissingHello));
        // A malformed hello is an error.
        let mut bad = Negotiation::from_alpn(Some(nuschl::ALPN));
        assert!(bad.version(&hello_body(&["BAD"]), server).is_err());
    }

    #[test]
    fn old_nuschl_clients_are_refused_from_the_date_and_stock_clients_never() {
        let settings = |extra: &str| {
            Config::from_toml(&format!("[history]\n{extra}"))
                .unwrap()
                .settings()
                .unwrap()
        };
        let agreed = |client_version: &str| {
            let mut negotiation = Negotiation::from_alpn(Some(nuschl::ALPN));
            negotiation
                .version(
                    &hello_body_from(&["history/1"], client_version),
                    server_capabilities(true, true, false),
                )
                .unwrap();
            negotiation
        };
        let graced = settings("minimum_client = \"0.3.0\"\nminimum_client_from = \"1970-01-02\"");
        let old = agreed("0.2.9");
        assert_eq!(
            old.refusal(&graced, 86_399),
            None,
            "admitted until the date"
        );
        assert_eq!(
            old.refusal(&graced, 86_400).as_deref(),
            Some("This server requires Nuschl 0.3.0 or newer. Update Nuschl to connect.")
        );
        assert_eq!(agreed("0.3.0").refusal(&graced, 86_400), None);
        assert_eq!(agreed("0.10.0").refusal(&graced, 86_400), None);
        assert!(agreed("0.3.0-rc.1").refusal(&graced, 86_400).is_some());
        // A client that reports no version (or garbage) is older than any minimum.
        assert!(agreed("").refusal(&graced, 86_400).is_some());
        assert!(agreed("new").refusal(&graced, 86_400).is_some());
        // Stock connections are never refused for their version.
        let mut stock = Negotiation::from_alpn(None);
        stock
            .version(
                &hello_body_from(&[], "0.0.1"),
                server_capabilities(true, true, false),
            )
            .unwrap();
        assert_eq!(stock.refusal(&graced, i64::MAX), None);
        // Without a minimum nobody is refused; without a date, at once.
        assert_eq!(old.refusal(&settings(""), i64::MAX), None);
        assert!(
            old.refusal(&settings("minimum_client = \"0.3.0\""), 0)
                .is_some()
        );
        // The hello announces the date only with a minimum.
        let hello = server_hello(&graced, None, false).policy.unwrap();
        assert_eq!(
            (hello.minimum_client, hello.minimum_client_from),
            (0x0000_0003_0000_0000, 86_400)
        );
    }

    #[test]
    fn at_most_sixteen_requests_are_in_flight_and_ids_are_unique() {
        let config = Config::default();
        let mut extension = Extension::new(
            server_capabilities(true, true, false),
            &config,
            Instant::now(),
        );
        for id in 0..16 {
            extension.begin(id).unwrap();
        }
        assert_eq!(extension.begin(3), Err(v1::error::Code::DuplicateRequest));
        assert_eq!(extension.begin(16), Err(v1::error::Code::TooManyRequests));
        extension.end(3);
        extension.begin(16).unwrap();
        assert_eq!(extension.begin(17), Err(v1::error::Code::TooManyRequests));
    }

    use prost::Message as _;
}
