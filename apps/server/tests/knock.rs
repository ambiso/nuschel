// SPDX-License-Identifier: EUPL-1.2
//! Real TLS/control/storage on private loopback; no external server or devices.
use bytes::BytesMut;
use nuschl_protocol::{
    ControlFrame, control as proto, decode_control, encode_control, kind,
    nuschl::{self, v1},
};
use nuschld::{
    acl::{AclEntry, perm},
    config::Config,
    server::{Running, start},
    state::Channel,
    storage::{Backend, MemoryStore},
};
use prost::Message;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

#[derive(Debug)]
struct Pin {
    fingerprint: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if nuschld::tls::sha256_hex(cert.as_ref()) != self.fingerprint {
            return Err(rustls::Error::General(
                "unexpected private fixture certificate".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        s: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            s,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        s: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            s,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct Client {
    tls: TlsStream<TcpStream>,
    buffer: BytesMut,
    session: u32,
}
impl Client {
    async fn login(server: &Running, name: &str, extension: bool) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin {
                fingerprint: server.fingerprint.clone(),
                provider,
            }))
            .with_no_client_auth();
        if extension {
            config.alpn_protocols = vec![nuschl::ALPN.to_vec()];
        }
        let tcp = TcpStream::connect(server.addresses[0]).await.unwrap();
        let tls = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let mut client = Self {
            tls,
            buffer: BytesMut::new(),
            session: 0,
        };
        let hello = extension.then(|| v1::Hello {
            protocol: nuschl::PROTOCOL,
            capabilities: vec!["history/1".into(), "knock/1".into()],
            ..Default::default()
        });
        let frame = nuschl::version_frame(
            &proto::Version {
                version_v2: Some((1 << 48) | (5 << 32)),
                ..Default::default()
            },
            hello.as_ref(),
        )
        .unwrap();
        client.tls.write_all(&frame).await.unwrap();
        client
            .send(
                kind::AUTHENTICATE,
                &proto::Authenticate {
                    username: Some(name.into()),
                    opus: Some(true),
                    ..Default::default()
                },
            )
            .await;
        client.session = proto::ServerSync::decode(client.until(kind::SERVER_SYNC).await.payload)
            .unwrap()
            .session
            .unwrap();
        client
    }
    async fn send(&mut self, kind: u16, message: &impl Message) {
        self.tls
            .write_all(&encode_control(kind, message).unwrap())
            .await
            .unwrap();
        self.tls.flush().await.unwrap();
    }
    async fn until(&mut self, kind: u16) -> ControlFrame {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(frame) = decode_control(&mut self.buffer).unwrap() {
                    if frame.kind == kind {
                        return frame;
                    }
                    assert_ne!(
                        frame.kind,
                        nuschl::kind::ERROR,
                        "unexpected protocol error: {:?}",
                        v1::Error::decode(frame.payload).unwrap()
                    );
                } else {
                    assert!(
                        self.tls.read_buf(&mut self.buffer).await.unwrap() > 0,
                        "connection closed"
                    );
                }
            }
        })
        .await
        .expect("private fixture response")
    }
    async fn post(&mut self, conversation: &[u8], id: u64, text: &str) -> u64 {
        self.send(
            nuschl::kind::CHAT_SEND,
            &v1::ChatSend {
                request_id: id,
                conversation: conversation.to_vec(),
                idempotency_key: vec![id as u8; 16],
                content: text.into(),
                format: v1::Format::Text as i32,
                ..Default::default()
            },
        )
        .await;
        v1::ChatAck::decode(self.until(nuschl::kind::CHAT_ACK).await.payload)
            .unwrap()
            .seq
    }
    async fn page(&mut self, conversation: &[u8], id: u64) -> v1::HistoryPage {
        self.send(
            nuschl::kind::HISTORY_REQUEST,
            &v1::HistoryRequest {
                request_id: id,
                conversation: conversation.to_vec(),
                anchor: Some(v1::history_request::Anchor::Latest(true)),
                limit: 100,
            },
        )
        .await;
        v1::HistoryPage::decode(self.until(nuschl::kind::HISTORY_PAGE).await.payload).unwrap()
    }
}

async fn fixture() -> (Running, tempfile::TempDir, Vec<u8>) {
    let directory = tempfile::tempdir().unwrap();
    let config = Config::from_toml(&format!("bind = [\"127.0.0.1\"]\nport=0\nstate_directory={:?}\n[autoban]\nattempts=0\n[password_hashing]\nmemory_kib=64\npasses=1\n", directory.path())).unwrap();
    let running = start(
        config.settings().unwrap(),
        Backend::Memory(MemoryStore::shared()),
    )
    .await
    .unwrap();
    let conversation = {
        let mut state = running.server.lock();
        let mut channel = Channel::new(1, Some(0), "Private".into());
        channel.node.acls.push(AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: None,
            group: "all".into(),
            grant: 0,
            deny: perm::ENTER | perm::SPEAK | perm::TEXT_MESSAGE,
        });
        let id = channel.conversation_id;
        state.conversations.add_channel(id, 1);
        state.channels.0.insert(1, channel);
        state.channels.get_mut(0).unwrap().children.push(1);
        state.invalidate();
        id.to_vec()
    };
    (running, directory, conversation)
}

#[tokio::test]
async fn negotiated_admission_reads_new_messages_only_over_real_tls_and_storage() {
    let (running, _directory, conversation) = fixture().await;
    let mut moderator = Client::login(&running, "Moderator", true).await;
    {
        let mut state = running.server.lock();
        state.users.get_mut(&moderator.session).unwrap().user_id = Some(0);
        state.invalidate();
    }
    let old = moderator.post(&conversation, 1, "before admission").await;
    let mut requester = Client::login(&running, "Requester", true).await;
    requester
        .send(
            nuschl::kind::KNOCK_REQUEST,
            &v1::KnockRequest {
                request_id: 2,
                channel: 1,
            },
        )
        .await;
    let pending =
        v1::KnockPending::decode(requester.until(nuschl::kind::KNOCK_PENDING).await.payload)
            .unwrap();
    let notice =
        v1::KnockNotice::decode(moderator.until(nuschl::kind::KNOCK_NOTICE).await.payload).unwrap();
    assert_eq!(notice.session, requester.session);
    assert_eq!(notice.knock_id, pending.knock_id);
    moderator
        .send(
            nuschl::kind::KNOCK_DECIDE,
            &v1::KnockDecide {
                request_id: 3,
                knock_id: notice.knock_id,
                admit: true,
            },
        )
        .await;
    moderator.until(nuschl::kind::DONE).await;
    let admitted =
        v1::KnockAdmission::decode(requester.until(nuschl::kind::KNOCK_ADMISSION).await.payload)
            .unwrap();
    assert!(admitted.active);
    assert_eq!(admitted.admitted_by, moderator.session);
    assert_eq!(admitted.first_seq, old + 1);
    let page = requester.page(&conversation, 4).await;
    assert!(page.messages.is_empty());
    assert!(page.restricted);
    let new = moderator.post(&conversation, 5, "after admission").await;
    let event =
        v1::ChatEvent::decode(requester.until(nuschl::kind::CHAT_EVENT).await.payload).unwrap();
    assert_eq!(event.message.unwrap().seq, new);
    let page = requester.page(&conversation, 6).await;
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].content, "after admission");
    assert_eq!(
        requester
            .post(&conversation, 7, "temporary text permission")
            .await,
        new + 1
    );
    requester
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(0),
                ..Default::default()
            },
        )
        .await;
    requester.until(nuschl::kind::KNOCK_ADMISSION).await;
    requester.page_after_leave_is_denied(&conversation).await;
    running.shutdown().await;
}

impl Client {
    async fn page_after_leave_is_denied(&mut self, conversation: &[u8]) {
        self.send(
            nuschl::kind::HISTORY_REQUEST,
            &v1::HistoryRequest {
                request_id: 8,
                conversation: conversation.to_vec(),
                anchor: Some(v1::history_request::Anchor::Latest(true)),
                limit: 10,
            },
        )
        .await;
        let error = v1::Error::decode(self.until(nuschl::kind::ERROR).await.payload).unwrap();
        assert_eq!(error.code, v1::error::Code::NotFound as i32);
    }
}

#[tokio::test]
async fn legacy_refused_enter_and_moderator_move_admit_without_new_messages() {
    let (running, _directory, _) = fixture().await;
    let mut moderator = Client::login(&running, "Moderator", false).await;
    {
        let mut state = running.server.lock();
        state.users.get_mut(&moderator.session).unwrap().user_id = Some(0);
        state.invalidate();
    }
    let mut requester = Client::login(&running, "Requester", false).await;
    requester
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(1),
                ..Default::default()
            },
        )
        .await;
    requester.until(kind::PERMISSION_DENIED).await;
    loop {
        let notice =
            proto::TextMessage::decode(moderator.until(kind::TEXT_MESSAGE).await.payload).unwrap();
        if notice.message.contains("knocks") {
            break;
        }
    }
    moderator
        .send(
            kind::USER_STATE,
            &proto::UserState {
                session: Some(requester.session),
                channel_id: Some(1),
                ..Default::default()
            },
        )
        .await;
    loop {
        let user =
            proto::UserState::decode(requester.until(kind::USER_STATE).await.payload).unwrap();
        if user.session == Some(requester.session) && user.channel_id == Some(1) {
            break;
        }
    }
    assert!(
        running
            .server
            .lock()
            .admission_floor(requester.session, 1)
            .is_some()
    );
    assert!(!running.server.lock().users[&requester.session].suppress);
    running.shutdown().await;
}
