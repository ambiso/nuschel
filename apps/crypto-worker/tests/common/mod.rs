// SPDX-License-Identifier: EUPL-1.2
//! The worker against nuschl-kt's directory in process, standing in for the server.
#![allow(dead_code)]

use nuschl_crypto_worker::{KeySource, Store, Worker};
use nuschl_identity::{DisplayName, NewIdentity, Validity};
use nuschl_ipc::crypto::{
    ContactView, CryptoRequest, CryptoResponse, CryptoServer, HeadOutcome, KtRecordBytes,
    PublishBundle,
};
use nuschl_kt::{
    AccountBinding, AccountId, AkdDirectory, Directory, DirectoryValue, Ed25519HeadSigner, Label,
    MemoryStore, RosterCommitment, SignedHead, VrfSecret,
};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// 2026-09-21, in milliseconds.
pub const T0: u64 = 1_790_000_000_000;
pub const SERVER: &str = "kt.example.org:64738";

pub type Dir = AkdDirectory<Arc<MemoryStore>>;

pub async fn directory(signer: [u8; 32]) -> Dir {
    AkdDirectory::open(
        Arc::new(MemoryStore::new()),
        VrfSecret::from_bytes([0x22; 32]),
        Box::new(Ed25519HeadSigner::from_seed(&signer)),
        T0,
    )
    .await
    .unwrap()
}

pub struct Client {
    pub worker: Worker,
    pub clock: Arc<AtomicU64>,
    pub account: [u8; 16],
    /// The server the next requests are about.
    pub server: &'static str,
    pub directory: tempfile::TempDir,
}

pub fn open_worker(path: &Path, clock: Arc<AtomicU64>) -> Worker {
    let store = Store::open(path, KeySource::File(path.join("store.key"))).unwrap();
    Worker::open(store)
        .unwrap()
        .with_clock(move || clock.load(Ordering::Relaxed))
}

impl Client {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let clock = Arc::new(AtomicU64::new(T0 + 1000));
        let worker = open_worker(directory.path(), clock.clone());
        Self {
            worker,
            clock,
            account: [0; 16],
            server: SERVER,
            directory,
        }
    }

    /// The same device after a restart: the worker reopens its directory.
    pub fn restart(self) -> Self {
        let Self {
            worker,
            clock,
            account,
            server,
            directory,
        } = self;
        drop(worker);
        let worker = open_worker(directory.path(), clock.clone());
        Self {
            worker,
            clock,
            account,
            server,
            directory,
        }
    }

    pub fn at(&self, ms: u64) {
        self.clock.store(ms, Ordering::Relaxed);
    }

    pub fn ask(&mut self, request: CryptoRequest) -> CryptoResponse {
        self.worker.handle(request)
    }

    pub fn connect(&mut self, dir: &Dir, account: [u8; 16], head: &SignedHead) -> HeadOutcome {
        self.account = account;
        match self.ask(CryptoRequest::Connect {
            witnessed: None,
            witnessing: Default::default(),
            registration: None,
            server: CryptoServer {
                key: self.server.into(),
                label: format!("Label of {}", self.server),
            },
            descriptor: dir.descriptor().encode(),
            account: account.to_vec(),
            head: head.encode(),
        }) {
            CryptoResponse::Head(outcome) => outcome,
            other => panic!("{other:?}"),
        }
    }

    /// Accepts the server's newest head, auditing from what it knew.
    pub async fn sync(&mut self, dir: &Dir) {
        let head = dir.latest_head().await.unwrap();
        match self.connect(dir, self.account, &head) {
            HeadOutcome::Accepted { .. } => {}
            HeadOutcome::NeedAudit { from, to } => {
                let audit = dir.audit(from, to).await.unwrap();
                match self.ask(CryptoRequest::Advance {
                    server: self.server.into(),
                    head: head.encode(),
                    audit: audit.encode().unwrap(),
                }) {
                    CryptoResponse::Head(HeadOutcome::Accepted { .. }) => {}
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    pub async fn contact(&mut self, dir: &Dir, person: &Person) -> ContactView {
        let lookup = dir.lookup(&person.label()).await.unwrap();
        match self.ask(CryptoRequest::Contact {
            witnessed: None,
            registration: Vec::new(),
            name_lookup: Vec::new(),
            name: None,
            server: self.server.into(),
            account: person.account.to_vec(),
            lookup: lookup.encode().unwrap(),
            record: Some(person.record.clone()),
            direct: true,
            user_id: None,
        }) {
            CryptoResponse::Contact(view) => view,
            other => panic!("{other:?}"),
        }
    }

    /// A lookup that named a registration number.
    pub async fn contact_as(&mut self, dir: &Dir, person: &Person, user_id: u32) -> ContactView {
        let lookup = dir.lookup(&person.label()).await.unwrap();
        match self.ask(CryptoRequest::Contact {
            witnessed: None,
            registration: Vec::new(),
            name_lookup: Vec::new(),
            name: None,
            server: self.server.into(),
            account: person.account.to_vec(),
            lookup: lookup.encode().unwrap(),
            record: Some(person.record.clone()),
            direct: false,
            user_id: Some(user_id),
        }) {
            CryptoResponse::Contact(view) => view,
            other => panic!("{other:?}"),
        }
    }

    pub async fn history(&mut self, dir: &Dir, person: &Person, bytes: Vec<u8>) -> ContactView {
        let _ = dir;
        match self.ask(CryptoRequest::ContactHistory {
            witnessed: None,
            server: self.server.into(),
            account: person.account.to_vec(),
            history: bytes,
        }) {
            CryptoResponse::Contact(view) => view,
            other => panic!("{other:?}"),
        }
    }

    /// Our binding from this worker, published as the server would.
    pub async fn publish_own(&mut self, dir: &Dir, at: u64) -> Person {
        let CryptoResponse::Bundle(bundle) = self.ask(CryptoRequest::PublishBundle {
            server: self.server.into(),
            device_name: "laptop".into(),
            recovery: nuschl_ipc::crypto::RecoveryPolicy::Never,
        }) else {
            panic!("no bundle")
        };
        let person = Person {
            account: self.account,
            value: value_of(&bundle),
            record: bundle.record.clone(),
        };
        person.publish(dir, at).await;
        person
    }
}

/// An identity bound to an account of the server, with the record behind it.
pub struct Person {
    pub account: [u8; 16],
    pub value: DirectoryValue,
    pub record: KtRecordBytes,
}

impl Person {
    pub fn new(dir: &Dir, account: u8) -> Self {
        let now_s = T0 / 1000;
        let validity = Validity::starting_at(now_s - 3600, 86_400 * 365).unwrap();
        let identity =
            NewIdentity::create(&DisplayName::new("laptop").unwrap(), validity, now_s).unwrap();
        let account = [account; 16];
        let statement = identity
            .device_signing
            .sign_account(dir.descriptor().log_id().0, account, now_s)
            .unwrap();
        let signing = identity.device_signing.certificate().clone();
        let devices = vec![identity.device_certificate.clone()];
        let roster = RosterCommitment::compute(&signing, &devices, &[]).unwrap();
        let value =
            AccountBinding::with_statement(identity.root.public_key().clone(), roster, &statement)
                .to_value();
        Self {
            account,
            value,
            record: KtRecordBytes {
                device_signing_certificate: signing.encode(),
                devices: devices.iter().map(|d| d.encode()).collect(),
                revocations: Vec::new(),
                statement: statement.encode(),
            },
        }
    }

    pub fn label(&self) -> Label {
        Label::Account(AccountId(self.account))
    }

    pub async fn publish(&self, dir: &Dir, at: u64) {
        dir.publish(vec![(self.label(), self.value.clone())], at)
            .await
            .unwrap();
    }
}

/// The value a bundle binds, as the server computes it.
pub fn value_of(bundle: &PublishBundle) -> DirectoryValue {
    let root = nuschl_identity::HybridPublicKey::decode(&bundle.root).unwrap();
    let signing = nuschl_identity::DeviceSigningCertificate::decode(
        &bundle.record.device_signing_certificate,
    )
    .unwrap();
    let devices: Vec<_> = bundle
        .record
        .devices
        .iter()
        .map(|d| nuschl_identity::DeviceCertificate::decode(d).unwrap())
        .collect();
    let statement = nuschl_identity::AccountStatement::decode(&bundle.record.statement).unwrap();
    let roster = RosterCommitment::compute(&signing, &devices, &[]).unwrap();
    AccountBinding::with_statement(root, roster, &statement).to_value()
}
