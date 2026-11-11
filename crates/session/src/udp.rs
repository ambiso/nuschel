// SPDX-License-Identifier: EUPL-1.2
//! Encrypted UDP voice with automatic fallback to the TLS tunnel.
//!
//! Stock servers send voice over UDP to a client once they receive a valid UDP
//! datagram from it, and switch back to the TCP tunnel when the client tunnels
//! a packet. Nuschl therefore treats UDP as usable only while the server's
//! ping replies arrive, and tunnels a ping after a failed probe so the server
//! stops sending UDP that never reaches us (e.g. inbound UDP is filtered).
use nuschl_protocol::{
    crypt::{CryptState, CryptStats},
    voice::{self, UdpMessage},
};
use std::net::SocketAddr;
use tokio::{
    net::UdpSocket,
    time::{Duration, Instant},
};

/// UDP stays active while replies arrive within this window.
const ACTIVE_WINDOW: Duration = Duration::from_secs(5);
/// Ping cadence while UDP is active, and while trying to establish it.
const ACTIVE_PING: Duration = Duration::from_secs(2);
const PROBE_PING: Duration = Duration::from_secs(1);
/// After this long without UDP, probe rarely to avoid flapping the server.
const GIVE_UP_AFTER: Duration = Duration::from_secs(15);
const BACKOFF_PING: Duration = Duration::from_secs(10);
/// Stock clients request a resync after five seconds without a good packet.
const RESYNC_AFTER: Duration = Duration::from_secs(5);
pub use nuschl_core::model::{PingStats, TransportStats};

pub const MAX_DATAGRAM: usize = 1024 + nuschl_protocol::crypt::HEADER;

pub enum Received {
    Message(UdpMessage),
    /// Decryption failed for long enough that the server should resync.
    RequestResync,
    Ignored,
}

pub struct Udp {
    socket: UdpSocket,
    crypt: Option<CryptState>,
    started: Instant,
    last_reply: Option<Instant>,
    last_good: Instant,
    last_resync_request: Option<Instant>,
    last_ping: Option<Instant>,
    probe_pending: Option<Instant>,
    /// UDP was answering at the previous `probe_failed` check.
    was_active: bool,
    pub stats: PingStats,
    pub forced_tcp: bool,
}

impl Udp {
    pub async fn bind(peer: SocketAddr, forced_tcp: bool) -> std::io::Result<Self> {
        let local: SocketAddr = if peer.is_ipv6() {
            "[::]:0".parse().map_err(std::io::Error::other)?
        } else {
            "0.0.0.0:0".parse().map_err(std::io::Error::other)?
        };
        let socket = UdpSocket::bind(local).await?;
        socket.connect(peer).await?;
        let now = Instant::now();
        Ok(Self {
            socket,
            crypt: None,
            started: now,
            last_reply: None,
            last_good: now,
            last_resync_request: None,
            last_ping: None,
            probe_pending: None,
            was_active: false,
            stats: PingStats::default(),
            forced_tcp,
        })
    }

    pub fn has_key(&self) -> bool {
        self.crypt.is_some()
    }

    pub fn set_key(&mut self, crypt: CryptState) {
        self.crypt = Some(crypt);
        self.last_good = Instant::now();
        self.last_ping = None;
    }

    pub fn set_decrypt_iv(&mut self, iv: &[u8]) -> bool {
        self.crypt
            .as_mut()
            .is_some_and(|crypt| crypt.set_decrypt_iv(iv).is_ok())
    }

    pub fn encrypt_iv(&self) -> Option<[u8; 16]> {
        self.crypt.as_ref().map(CryptState::encrypt_iv)
    }

    pub fn crypt_stats(&self) -> CryptStats {
        self.crypt
            .as_ref()
            .map(|crypt| crypt.stats)
            .unwrap_or_default()
    }

    pub fn active(&self) -> bool {
        !self.forced_tcp
            && self.crypt.is_some()
            && self
                .last_reply
                .is_some_and(|last| last.elapsed() < ACTIVE_WINDOW)
    }

    /// Voice goes over UDP only while the server is answering UDP pings.
    pub async fn send_voice(&mut self, payload: &[u8]) -> bool {
        if !self.active() {
            return false;
        }
        self.send(payload).await
    }

    async fn send(&mut self, plain: &[u8]) -> bool {
        let Some(crypt) = self.crypt.as_mut() else {
            return false;
        };
        let packet = crypt.encrypt(plain);
        self.socket.send(&packet).await.is_ok()
    }

    /// Send a UDP ping when one is due; returns whether a probe was sent.
    pub async fn maybe_ping(&mut self, version: u64, now_ms: u64) -> bool {
        if self.forced_tcp || self.crypt.is_none() {
            return false;
        }
        let since_reply = self
            .last_reply
            .map_or_else(|| self.started.elapsed(), |last| last.elapsed());
        let interval = if self.active() {
            ACTIVE_PING
        } else if since_reply > GIVE_UP_AFTER {
            BACKOFF_PING
        } else {
            PROBE_PING
        };
        if self.last_ping.is_some_and(|last| last.elapsed() < interval) {
            return false;
        }
        self.last_ping = Some(Instant::now());
        // Keep the oldest unanswered probe: probes go out every second, so
        // restarting the clock on each one would never let a probe count as
        // failed, and the server would keep sending voice by UDP.
        if !self.active() && self.probe_pending.is_none() {
            self.probe_pending = Some(Instant::now());
        }
        self.send(&voice::encode_ping(version, now_ms)).await
    }

    /// Whether the caller should tunnel a ping over TCP now, so the server stops
    /// sending voice by UDP that never arrives: right when UDP stops answering
    /// (five seconds without a reply already proves it), and again whenever a
    /// later probe goes unanswered (the server may have seen it).
    pub fn probe_failed(&mut self) -> bool {
        let active = self.active();
        let lost = std::mem::replace(&mut self.was_active, active) && !active;
        if lost {
            self.probe_pending = None;
            return true;
        }
        match self.probe_pending {
            Some(sent) if sent.elapsed() > Duration::from_millis(1500) && !active => {
                self.probe_pending = None;
                true
            }
            _ => false,
        }
    }

    pub async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.socket.recv(buffer).await
    }

    pub fn handle(&mut self, datagram: &[u8], now_ms: u64) -> Received {
        let Some(crypt) = self.crypt.as_mut() else {
            return Received::Ignored;
        };
        let Some(plain) = crypt.decrypt(datagram) else {
            if self.last_good.elapsed() > RESYNC_AFTER
                && self
                    .last_resync_request
                    .is_none_or(|last| last.elapsed() > RESYNC_AFTER)
            {
                self.last_resync_request = Some(Instant::now());
                return Received::RequestResync;
            }
            return Received::Ignored;
        };
        self.last_good = Instant::now();
        match voice::decode_udp(&plain) {
            Ok(UdpMessage::Ping { timestamp }) => {
                self.last_reply = Some(Instant::now());
                self.probe_pending = None;
                if timestamp <= now_ms {
                    self.stats.record((now_ms - timestamp) as f32);
                }
                Received::Message(UdpMessage::Ping { timestamp })
            }
            Ok(message) => {
                self.last_reply = Some(Instant::now());
                Received::Message(message)
            }
            Err(_) => Received::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ping_statistics_track_average_and_ignore_invalid_samples() {
        let mut stats = PingStats::default();
        for sample in [10., 20., f32::NAN, -1., 30.] {
            stats.record(sample);
        }
        assert_eq!(stats.packets, 3);
        assert!((stats.average_ms - 20.).abs() < 0.01);
        assert!(stats.variance_ms > 0.);
    }

    #[tokio::test]
    async fn udp_voice_waits_for_a_ping_reply_and_round_trips_through_crypt() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut client = Udp::bind(server.local_addr().unwrap(), false)
            .await
            .unwrap();
        let key = [3; 16];
        let (client_nonce, server_nonce) = ([1; 16], [2; 16]);
        client.set_key(CryptState::client(&key, &client_nonce, &server_nonce).unwrap());
        let mut server_crypt = CryptState::server(&key, &client_nonce, &server_nonce).unwrap();
        assert!(!client.send_voice(b"early").await, "no UDP before a reply");
        assert!(client.maybe_ping(voice::PROTOBUF_VERSION, 1000).await);
        let mut buffer = [0; MAX_DATAGRAM];
        let (size, from) = server.recv_from(&mut buffer).await.unwrap();
        let ping = server_crypt.decrypt(&buffer[..size]).unwrap();
        assert_eq!(
            voice::decode_udp(&ping).unwrap(),
            UdpMessage::Ping { timestamp: 1000 }
        );
        server
            .send_to(&server_crypt.encrypt(&ping), from)
            .await
            .unwrap();
        let size = client.recv(&mut buffer).await.unwrap();
        assert!(matches!(
            client.handle(&buffer[..size], 1012),
            Received::Message(UdpMessage::Ping { timestamp: 1000 })
        ));
        assert!(client.active());
        assert_eq!(client.stats.packets, 1);
        assert!((client.stats.average_ms - 12.).abs() < 0.01);
        assert!(client.send_voice(b"voice").await);
        let (size, _) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(server_crypt.decrypt(&buffer[..size]).unwrap(), b"voice");
    }

    /// Inbound UDP blocked: probes go unanswered once a second, as the session
    /// sends them. The caller must be told to tunnel within a few seconds, and
    /// again regularly while UDP stays dead (the server flips back on UDP audio).
    #[tokio::test(start_paused = true)]
    async fn unanswered_probes_ask_for_a_tunnel_ping_promptly() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut client = Udp::bind(silent.local_addr().unwrap(), false)
            .await
            .unwrap();
        client.set_key(CryptState::client(&[3; 16], &[1; 16], &[2; 16]).unwrap());
        let mut tunnelled = Vec::new();
        for second in 0..12 {
            client
                .maybe_ping(voice::PROTOBUF_VERSION, second * 1000)
                .await;
            if client.probe_failed() {
                tunnelled.push(second);
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(
            tunnelled.first(),
            Some(&2),
            "first tunnel ping too late: {tunnelled:?}"
        );
        assert!(
            tunnelled.len() >= 4,
            "tunnel pings should repeat while UDP is dead: {tunnelled:?}"
        );
    }

    /// UDP worked, then inbound UDP is cut: the tunnel ping must go out on the
    /// first check after UDP counts as lost, not after another probe cycle.
    #[tokio::test(start_paused = true)]
    async fn losing_udp_asks_for_a_tunnel_ping_immediately() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut client = Udp::bind(server.local_addr().unwrap(), false)
            .await
            .unwrap();
        let key = [3; 16];
        let (client_nonce, server_nonce) = ([1; 16], [2; 16]);
        client.set_key(CryptState::client(&key, &client_nonce, &server_nonce).unwrap());
        let mut server_crypt = CryptState::server(&key, &client_nonce, &server_nonce).unwrap();
        assert!(client.maybe_ping(voice::PROTOBUF_VERSION, 1000).await);
        let mut buffer = [0; MAX_DATAGRAM];
        let (size, from) = server.recv_from(&mut buffer).await.unwrap();
        let ping = server_crypt.decrypt(&buffer[..size]).unwrap();
        server
            .send_to(&server_crypt.encrypt(&ping), from)
            .await
            .unwrap();
        let size = client.recv(&mut buffer).await.unwrap();
        client.handle(&buffer[..size], 1001);
        assert!(client.active());
        assert!(!client.probe_failed(), "UDP is fine");
        let mut seconds = 0;
        while client.active() {
            tokio::time::advance(Duration::from_secs(1)).await;
            seconds += 1;
        }
        assert!(
            client.probe_failed(),
            "no tunnel ping when UDP was lost after {seconds} s"
        );
        assert!(!client.probe_failed(), "the loss is reported once");
    }

    #[tokio::test]
    async fn forced_tcp_never_uses_udp() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut client = Udp::bind(server.local_addr().unwrap(), true).await.unwrap();
        client.set_key(CryptState::client(&[0; 16], &[0; 16], &[0; 16]).unwrap());
        assert!(!client.maybe_ping(0, 1).await);
        assert!(!client.send_voice(b"x").await);
    }
}
