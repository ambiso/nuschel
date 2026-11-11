// SPDX-License-Identifier: EUPL-1.2
//! The Nuschel extension's negotiation (docs/extension.md §1): it is on if and
//! only if ALPN selected `nuschel/1` and both hellos are present; the client's
//! hello is accepted once, before `Authenticate`, and the capability set is
//! then frozen for the connection. Anything else fails closed.
use crate::{
    config::{Config, GuestHistory, Settings},
    limits::{ByteBucket, LeakyBucket},
};
use nuschel_protocol::{
    control as proto,
    nuschel::{self, Id, capability, v1},
};
use std::{
    collections::{BTreeSet, HashMap},
    time::Instant,
};

/// A set of the capabilities this server implements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities(u8);

impl Capabilities {
    fn bit(name: &str) -> u8 {
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

/// ALPN selected `nuschel/1` but the client sent no hello before `Authenticate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissingHello;

/// What a connection has negotiated before authenticating.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Negotiation {
    /// No `nuschel/1` ALPN: a stock connection; field 7200 is ignored.
    #[default]
    Stock,
    /// ALPN selected `nuschel/1`, but the client's hello has not arrived.
    Offered,
    /// Both hellos are present; frozen.
    Agreed(Capabilities),
}

impl Negotiation {
    pub fn from_alpn(alpn: Option<&[u8]>) -> Self {
        if alpn == Some(nuschel::ALPN) {
            Self::Offered
        } else {
            Self::Stock
        }
    }

    /// Apply a `Version` received before authentication. Only the first hello
    /// counts; later ones cannot change the capabilities. `Err` closes the
    /// connection (a malformed hello on a `nuschel/1` connection).
    pub fn version(
        &mut self,
        body: &[u8],
        server: Capabilities,
    ) -> Result<(), nuschel::HelloError> {
        if *self != Self::Offered {
            return Ok(());
        }
        if let Some(hello) = nuschel::hello(body)? {
            let theirs = Capabilities::from_names(hello.capabilities.iter().map(String::as_str));
            *self = Self::Agreed(Capabilities(theirs.0 & server.0));
        }
        Ok(())
    }

    /// At `Authenticate`: the extension's capabilities, `Ok(None)` for a stock
    /// connection, or `Err` when ALPN said `nuschel/1` but no hello came.
    pub fn freeze(self) -> Result<Option<Capabilities>, MissingHello> {
        match self {
            Self::Stock => Ok(None),
            Self::Offered => Err(MissingHello),
            Self::Agreed(capabilities) => Ok(Some(capabilities)),
        }
    }
}

/// A connection's extension state: what it negotiated, its requests in
/// flight, subscriptions and the extension's own rate limits.
pub struct Extension {
    pub capabilities: Capabilities,
    /// Request ids in flight; at most [`nuschel::MAX_OUTSTANDING`].
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
        if self.outstanding.len() >= nuschel::MAX_OUTSTANDING {
            return Err(v1::error::Code::TooManyRequests);
        }
        self.outstanding.push(request_id);
        Ok(())
    }

    pub fn end(&mut self, request_id: u64) {
        self.outstanding.retain(|id| *id != request_id);
    }
}

/// What this server offers: everything, and `kt/1` only with a key-transparency
/// log.
pub fn server_capabilities(kt: bool) -> Capabilities {
    Capabilities::from_names(
        capability::ALL
            .into_iter()
            .filter(|name| kt || *name != capability::KT),
    )
}

/// The server's hello, sent in its `Version` to `nuschel/1` connections, with
/// the descriptor of its key-transparency log if it has one.
pub fn server_hello(settings: &Settings, kt_log: Option<&[u8]>) -> v1::Hello {
    let history = &settings.config.history;
    v1::Hello {
        protocol: nuschel::PROTOCOL,
        capabilities: server_capabilities(kt_log.is_some()).names(),
        kt_log: kt_log.map(<[u8]>::to_vec).unwrap_or_default(),
        policy: Some(v1::Policy {
            mode: v1::policy::Mode::Compatible as i32,
            minimum_client: settings.minimum_client.unwrap_or(0),
            recommended_client: settings.recommended_client.unwrap_or(0),
            retention_days: history.retention_days,
            guests: match history.guests {
                GuestHistory::None => v1::policy::Guests::None,
                GuestHistory::Full => v1::policy::Guests::Full,
            } as i32,
            edit_limit: history.edit_limit,
        }),
    }
}

/// The server's `Version`, with or without the hello.
pub fn version_frame(version: &proto::Version, hello: Option<&v1::Hello>) -> bytes::Bytes {
    nuschel::version_frame(version, hello).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschel_protocol::HEADER_BYTES;

    fn hello_body(capabilities: &[&str]) -> Vec<u8> {
        let hello = v1::Hello {
            protocol: nuschel::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
        };
        nuschel::version_frame(&proto::Version::default(), Some(&hello)).unwrap()[HEADER_BYTES..]
            .to_vec()
    }

    #[test]
    fn the_extension_needs_alpn_and_a_hello() {
        let server = server_capabilities(true);
        // ALPN without a hello fails closed.
        assert_eq!(
            Negotiation::from_alpn(Some(nuschel::ALPN)).freeze(),
            Err(MissingHello)
        );
        // A hello without ALPN is ignored: a stock connection.
        let mut stock = Negotiation::from_alpn(None);
        stock.version(&hello_body(&["history/1"]), server).unwrap();
        assert_eq!(stock.freeze(), Ok(None));
        let mut mumble = Negotiation::from_alpn(Some(nuschel::ALPN_MUMBLE));
        mumble.version(&hello_body(&["history/1"]), server).unwrap();
        assert_eq!(mumble.freeze(), Ok(None));
        // Both: the intersection, frozen at the first hello.
        let mut agreed = Negotiation::from_alpn(Some(nuschel::ALPN));
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
        // A plain Version on a nuschel/1 connection leaves it waiting.
        let mut waiting = Negotiation::from_alpn(Some(nuschel::ALPN));
        waiting
            .version(&proto::Version::default().encode_to_vec(), server)
            .unwrap();
        assert_eq!(waiting.freeze(), Err(MissingHello));
        // A malformed hello is an error.
        let mut bad = Negotiation::from_alpn(Some(nuschel::ALPN));
        assert!(bad.version(&hello_body(&["BAD"]), server).is_err());
    }

    #[test]
    fn at_most_sixteen_requests_are_in_flight_and_ids_are_unique() {
        let config = Config::default();
        let mut extension = Extension::new(server_capabilities(true), &config, Instant::now());
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
