// SPDX-License-Identifier: EUPL-1.2
//! Device linking (docs/e2ee.md §3.4) and the identity's other devices.
//!
//! - **Linking** runs nuschel-identity's protocol (SPAKE2 over the short code, key
//!   confirmation, an X-Wing-sealed provisioning bundle). The caller relays the four
//!   messages between the devices; the server's linking mailbox comes later. The
//!   state between steps is kept in memory only: a link takes a minute, and a crash
//!   simply means starting over with a new code.
//! - **What the bundle carries** besides the identity: the device-signing key (every
//!   linked desktop may link others, open question 2's default), the recovery blob
//!   and device-sync key if this device has them, and as application data (JSON) the
//!   identity's known device certificates, the pinned logs with our accounts and
//!   account statements, and the identities verified out of band. The new device
//!   therefore inherits pins (§7.2) and publishes the same binding value as the
//!   existing one.
//! - **Own devices.** The worker keeps the identity's device certificates
//!   ([`State::own_devices`](crate::state::State)): from linking, and from verified
//!   lookups of our own account. A publication lists all of them, so devices do not
//!   take each other out of the roster, and the value another of our devices
//!   publishes is one this device knows: self-monitoring accepts it instead of raising
//!   the own-key alarm (a value from a lookup of our own account is accepted only
//!   when its record verifies under our identity, with a statement our device-signing
//!   key made for this account and log).
use crate::state::{MAX_PUBLISHED, ServerState, Verified};
use crate::worker::Worker;
use nuschel_identity::{
    AccountStatement, DeviceCertificate, DeviceKeys, DeviceKeystore, DisplayName,
    ExistingDeviceLink, LinkBundle, LinkCode, LinkDeviceKeys, LinkPake, NewAwaitingBundle,
    NewDeviceLink, ProvisioningBundle, Validity, VerifiedIdentity,
};
use nuschel_ipc::crypto::{CryptoResponse, KtRecordBytes, base64_bytes, base64_list};
use nuschel_kt::{AccountBinding, LogDescriptor, RosterCommitment};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Device certificates kept for the identity (the KT roster allows 16).
pub const MAX_OWN_DEVICES: usize = 16;
const DEVICE_VALIDITY_S: u64 = 365 * 86_400;

/// A link in progress, in memory.
#[derive(Default)]
pub struct Pending {
    existing: Option<ExistingDeviceLink>,
    new: Option<(NewAwaitingBundle, DeviceKeys, String)>,
}

/// The worker's part of a provisioning bundle.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkData {
    #[serde(with = "base64_list")]
    devices: Vec<Vec<u8>>,
    servers: Vec<LinkedServer>,
    verified: BTreeMap<String, Verified>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedServer {
    key: String,
    label: String,
    #[serde(with = "base64_bytes")]
    descriptor: Vec<u8>,
    #[serde(with = "base64_bytes")]
    account: Vec<u8>,
    #[serde(with = "base64_bytes")]
    statement: Vec<u8>,
}

/// The newest valid certificate per device under `identity`, this device's own
/// (from its keystore) first.
fn newest_devices(
    identity: &VerifiedIdentity,
    own: &DeviceCertificate,
    known: impl IntoIterator<Item = DeviceCertificate>,
    now_s: u64,
) -> Vec<DeviceCertificate> {
    let mut by_device: BTreeMap<[u8; 16], DeviceCertificate> = BTreeMap::new();
    by_device.insert(*own.device_id().as_bytes(), own.clone());
    for certificate in known {
        if certificate.verify(identity).is_err() || certificate.validity().check(now_s).is_err() {
            continue;
        }
        let id = *certificate.device_id().as_bytes();
        if id == *own.device_id().as_bytes() {
            continue;
        }
        match by_device.get(&id) {
            Some(known) if known.key_epoch() >= certificate.key_epoch() => {}
            _ => {
                by_device.insert(id, certificate);
            }
        }
    }
    let mut devices: Vec<DeviceCertificate> = by_device.into_values().collect();
    devices.truncate(MAX_OWN_DEVICES);
    devices
}

/// The `acct:` value a publication of `devices` with `statement` makes.
pub(crate) fn binding_value(
    identity: &VerifiedIdentity,
    devices: &[DeviceCertificate],
    statement: &AccountStatement,
) -> Result<Vec<u8>, String> {
    let roster = RosterCommitment::compute(identity.device_signing_certificate(), devices, &[])
        .map_err(|error| error.to_string())?;
    Ok(
        AccountBinding::with_statement(identity.root_key().clone(), roster, statement)
            .to_value()
            .encode(),
    )
}

fn remember(published: &mut Vec<Vec<u8>>, value: Vec<u8>) {
    if !published.contains(&value) {
        // Kept in front of what this device made last: `published.last()` stays the
        // value this device itself wants current.
        let at = published.len().saturating_sub(1);
        published.insert(at, value);
        if published.len() > MAX_PUBLISHED {
            published.remove(0);
        }
    }
}

impl Worker {
    /// The identity's devices to publish: this device and every other one known.
    pub(crate) fn own_device_certificates(&self) -> Vec<DeviceCertificate> {
        let Some(keystore) = &self.keystore else {
            return Vec::new();
        };
        let known = self
            .state
            .own_devices
            .iter()
            .filter_map(|bytes| DeviceCertificate::decode(bytes).ok());
        newest_devices(
            keystore.identity(),
            keystore.device_certificate(),
            known,
            self.now() / 1000,
        )
    }

    fn keep_own_devices(&mut self, devices: &[DeviceCertificate]) {
        self.state.own_devices = devices
            .iter()
            .take(MAX_OWN_DEVICES)
            .map(DeviceCertificate::encode)
            .collect();
    }

    /// A verified lookup of our own account (its record verified under our identity,
    /// with a statement for this account and log): learn the devices and the
    /// statement, and accept the value in self-monitoring (another of our devices may
    /// have published it).
    pub(crate) fn learn_own_record(
        &mut self,
        key: &str,
        record: &KtRecordBytes,
        value: Vec<u8>,
    ) -> Result<(), String> {
        let Some(keystore) = &self.keystore else {
            return Ok(());
        };
        let identity = keystore.identity().clone();
        let own = keystore.device_certificate().clone();
        let known: Vec<DeviceCertificate> = self
            .state
            .own_devices
            .iter()
            .chain(&record.devices)
            .filter_map(|bytes| DeviceCertificate::decode(bytes).ok())
            .collect();
        let devices = newest_devices(&identity, &own, known, self.now() / 1000);
        self.keep_own_devices(&devices);
        let statement = AccountStatement::decode(&record.statement).ok();
        let entry = self.server_mut(key)?;
        if let Some(statement) = statement
            && statement.verify(&identity).is_ok()
            && statement.account().as_slice() == entry.account.as_slice()
            && entry.statement.is_empty()
        {
            entry.statement = statement.encode();
        }
        remember(&mut entry.published, value);
        Ok(())
    }

    // ---- the existing device ----

    pub(crate) fn link_offer(&mut self) -> Result<CryptoResponse, String> {
        let keystore = self
            .keystore
            .as_ref()
            .ok_or("there is no identity to link")?;
        if keystore.device_signing().is_none() {
            return Err("this device cannot link others (no device-signing key)".into());
        }
        let mut nameplate = [0u8; 4];
        getrandom_nameplate(&mut nameplate)?;
        let code = LinkCode::generate(u32::from_be_bytes(nameplate) % 1000)
            .map_err(|error| error.to_string())?;
        let (link, pake) = ExistingDeviceLink::start(&code);
        self.link.existing = Some(link);
        Ok(CryptoResponse::LinkOffer {
            code: code.as_str().to_owned(),
            pake: pake.encode(),
        })
    }

    pub(crate) fn link_approve(
        &mut self,
        pake: &[u8],
        device_keys: &[u8],
    ) -> Result<CryptoResponse, String> {
        let link = self
            .link
            .existing
            .take()
            .ok_or("no link in progress: offer a new code")?;
        let pake = LinkPake::decode(pake).map_err(|error| error.to_string())?;
        let message = LinkDeviceKeys::decode(device_keys).map_err(|error| error.to_string())?;
        let new = link
            .receive_pake(&pake)
            .and_then(|awaiting| awaiting.receive_device_keys(&message))
            .map_err(|_| "the code was wrong or the messages were changed; start over")?;
        let now_s = self.now() / 1000;
        let keystore = self.keystore.as_ref().ok_or("no identity")?;
        let authority = keystore
            .device_signing()
            .ok_or("this device cannot link others (no device-signing key)")?;
        let validity = Validity::starting_at(now_s.saturating_sub(3600), DEVICE_VALIDITY_S)
            .map_err(|error| error.to_string())?;
        let (certificate, credential) = authority
            .certify_device(new.device_keys(), new.display_name(), validity)
            .map_err(|error| error.to_string())?;
        let identity = keystore.identity().clone();
        let mut devices = self.own_device_certificates();
        devices.push(certificate.clone());
        let devices = newest_devices(&identity, keystore.device_certificate(), devices, now_s);
        let data = LinkData {
            devices: devices.iter().map(DeviceCertificate::encode).collect(),
            servers: self
                .state
                .servers
                .iter()
                .map(|(key, entry)| LinkedServer {
                    key: key.clone(),
                    label: entry.label.clone(),
                    descriptor: entry.descriptor.clone(),
                    account: entry.account.clone(),
                    statement: entry.statement.clone(),
                })
                .collect(),
            verified: self.state.verified.clone(),
        };
        let data = serde_json::to_vec(&data).map_err(|error| error.to_string())?;
        let mut bundle = ProvisioningBundle::new(&identity, certificate, credential)
            .with_device_signing(authority)
            .map_err(|error| error.to_string())?
            .with_application_data(&data)
            .map_err(|error| error.to_string())?;
        if let Some(blob) = keystore.recovery_blob() {
            bundle = bundle.with_recovery_blob(blob.clone());
        }
        if let Some(key) = keystore.sync_key() {
            bundle = bundle.with_sync_key(key);
        }
        let sealed = new.seal(&bundle).map_err(|error| error.to_string())?;
        // The value the new device will publish (the same roster and statement) is
        // ours too: self-monitoring must not take it for someone else's.
        for entry in self.state.servers.values_mut() {
            if let Ok(statement) = AccountStatement::decode(&entry.statement)
                && entry.account.len() == 16
                && let Ok(value) = binding_value(&identity, &devices, &statement)
            {
                remember(&mut entry.published, value);
            }
        }
        self.keep_own_devices(&devices);
        self.save()?;
        Ok(CryptoResponse::LinkBundle(sealed.encode()))
    }

    // ---- the new device ----

    pub(crate) fn link_join(
        &mut self,
        code: &str,
        pake: &[u8],
        device_name: &str,
    ) -> Result<CryptoResponse, String> {
        if self.keystore.is_some() {
            return Err("this device already has an identity".into());
        }
        let code = LinkCode::parse(code).map_err(|error| error.to_string())?;
        let theirs = LinkPake::decode(pake).map_err(|error| error.to_string())?;
        let name = DisplayName::new(if device_name.trim().is_empty() {
            "Nuschel"
        } else {
            device_name.trim()
        })
        .map_err(|error| format!("device name: {error}"))?;
        let device = DeviceKeys::generate().map_err(|error| error.to_string())?;
        let (link, own) = NewDeviceLink::start(&code, &device, name.clone());
        let (awaiting, keys) = link
            .receive_pake(&theirs)
            .map_err(|_| "the link messages do not fit this code")?;
        self.link.new = Some((awaiting, device, name.as_str().to_owned()));
        Ok(CryptoResponse::LinkJoin {
            pake: own.encode(),
            device_keys: keys.encode(),
        })
    }

    pub(crate) fn link_complete(&mut self, bundle: &[u8]) -> Result<CryptoResponse, String> {
        if self.keystore.is_some() {
            return Err("this device already has an identity".into());
        }
        let (awaiting, device, name) = self
            .link
            .new
            .take()
            .ok_or("no link in progress on this device")?;
        let bundle = LinkBundle::decode(bundle).map_err(|error| error.to_string())?;
        let provisioned = awaiting
            .receive_bundle(&device, &bundle, self.now() / 1000)
            .map_err(|_| "the bundle does not come from this link session")?;
        let data: LinkData = if provisioned.application_data.is_empty() {
            LinkData::default()
        } else {
            serde_json::from_slice(&provisioned.application_data)
                .map_err(|error| format!("the bundle's data: {error}"))?
        };
        let keystore = DeviceKeystore::from_provisioned(device, provisioned)
            .map_err(|error| error.to_string())?;
        self.store
            .save_keystore(&keystore)
            .map_err(|error| format!("cannot save the keystore: {error}"))?;
        self.keystore = Some(keystore);
        self.state.device_name = name;
        for server in data.servers.into_iter().take(crate::state::MAX_SERVERS) {
            if LogDescriptor::decode(&server.descriptor).is_err() {
                continue;
            }
            self.state.servers.insert(
                server.key,
                ServerState {
                    label: server.label,
                    descriptor: server.descriptor,
                    account: server.account,
                    statement: server.statement,
                    ..ServerState::default()
                },
            );
        }
        for (identity, verified) in data.verified {
            self.state.verified.entry(identity).or_insert(verified);
        }
        let known: Vec<DeviceCertificate> = data
            .devices
            .iter()
            .filter_map(|bytes| DeviceCertificate::decode(bytes).ok())
            .collect();
        if let Some(keystore) = &self.keystore {
            let devices = newest_devices(
                keystore.identity(),
                keystore.device_certificate(),
                known,
                self.now() / 1000,
            );
            self.keep_own_devices(&devices);
        }
        self.save()?;
        Ok(CryptoResponse::Identity(self.identity_view(true)))
    }
}

/// A random nameplate (no mailbox assigns one yet).
fn getrandom_nameplate(out: &mut [u8; 4]) -> Result<(), String> {
    let code = LinkCode::generate(0).map_err(|error| error.to_string())?;
    // The code's eight random digits: reuse them rather than add a dependency.
    let digits: String = code.as_str().chars().filter(char::is_ascii_digit).collect();
    let number: u32 = digits.get(1..9).and_then(|d| d.parse().ok()).unwrap_or(0);
    *out = number.to_be_bytes();
    Ok(())
}
