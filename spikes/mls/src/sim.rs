// SPDX-License-Identifier: EUPL-1.2
//! A client driving one conversation through the [`DeliveryService`], as the
//! crypto worker and session would: every send waits for the server's ack (`seq`);
//! commits are merged only once the server accepted them, and cleared otherwise.

use std::collections::HashSet;

use openmls::messages::group_info::GroupInfo;
use openmls::prelude::*;

use crate::ds::{DeliveryService, DsError};
use crate::pins::ConversationPins;
use crate::{ConversationBinding, Device, Received, member_device, to_wire};

pub struct Client {
    pub device: Device,
    pub group: Option<MlsGroup>,
    /// The last `seq` processed.
    pub cursor: u64,
    own: HashSet<u64>,
}

impl Client {
    pub fn new(device: Device) -> Self {
        Self {
            device,
            group: None,
            cursor: 0,
            own: HashSet::new(),
        }
    }

    pub fn group(&mut self) -> &mut MlsGroup {
        self.group.as_mut().expect("in the group")
    }

    pub fn group_id(&self) -> GroupId {
        self.group
            .as_ref()
            .expect("in the group")
            .group_id()
            .clone()
    }

    pub fn create(&mut self, ds: &mut DeliveryService, binding: &ConversationBinding) {
        let group = self.device.create_group(binding);
        let info = group
            .export_group_info(self.device.provider.crypto(), self.device.signer(), false)
            .expect("group info");
        ds.create_group(&to_wire(&info), group.export_ratchet_tree().into())
            .expect("ds accepts the group");
        self.group = Some(group);
    }

    pub fn submit_commit(
        &mut self,
        ds: &mut DeliveryService,
        commit: MlsMessageOut,
        info: Option<GroupInfo>,
    ) -> Result<u64, DsError> {
        let info = info.map(|i| to_wire(&MlsMessageOut::from(i)));
        match ds.submit(&to_wire(&commit), info.as_deref()) {
            Ok(seq) => {
                self.own.insert(seq);
                let provider = &self.device.provider;
                self.group
                    .as_mut()
                    .unwrap()
                    .merge_pending_commit(provider)
                    .expect("merge own commit");
                Ok(seq)
            }
            Err(e) => {
                let provider = &self.device.provider;
                self.group
                    .as_mut()
                    .unwrap()
                    .clear_pending_commit(provider.storage())
                    .expect("clear pending commit");
                Err(e)
            }
        }
    }

    /// Adds devices by key package; returns the Welcome for them.
    pub fn add(
        &mut self,
        ds: &mut DeliveryService,
        kps: &[KeyPackage],
    ) -> Result<Vec<u8>, DsError> {
        let (commit, welcome, info) = {
            let provider = &self.device.provider;
            let signer = &self.device.keys.signer;
            self.group
                .as_mut()
                .unwrap()
                .add_members(provider, signer, kps)
                .expect("add")
        };
        self.submit_commit(ds, commit, info)?;
        Ok(to_wire(&welcome))
    }

    pub fn remove(&mut self, ds: &mut DeliveryService, name: &str) -> Result<u64, DsError> {
        let leaf = Device::leaf_of(self.group.as_ref().unwrap(), name);
        let (commit, _, info) = {
            let provider = &self.device.provider;
            let signer = &self.device.keys.signer;
            self.group
                .as_mut()
                .unwrap()
                .remove_members(provider, signer, &[leaf])
                .expect("remove")
        };
        self.submit_commit(ds, commit, info)
    }

    /// A commit with a fresh update path: new leaf HPKE key and path secrets.
    pub fn update(&mut self, ds: &mut DeliveryService) -> Result<u64, DsError> {
        let bundle = {
            let provider = &self.device.provider;
            let signer = &self.device.keys.signer;
            self.group
                .as_mut()
                .unwrap()
                .self_update(provider, signer, LeafNodeParameters::default())
                .expect("self update")
        };
        let (commit, _, info) = bundle.into_contents();
        self.submit_commit(ds, commit, info)
    }

    /// Stages a self-update commit without submitting it (for concurrency tests).
    pub fn stage_update(&mut self) -> (MlsMessageOut, Option<GroupInfo>) {
        let provider = &self.device.provider;
        let signer = &self.device.keys.signer;
        let bundle = self
            .group
            .as_mut()
            .unwrap()
            .self_update(provider, signer, LeafNodeParameters::default())
            .expect("self update");
        let (commit, _, info) = bundle.into_contents();
        (commit, info)
    }

    pub fn send(&mut self, ds: &mut DeliveryService, text: &str) -> Result<u64, DsError> {
        let bytes = {
            let device = &self.device;
            device.send(self.group.as_mut().unwrap(), text)
        };
        let seq = ds.submit(&bytes, None)?;
        self.own.insert(seq);
        Ok(seq)
    }

    /// Fetches and processes everything after the cursor, in `seq` order.
    pub fn sync(&mut self, ds: &DeliveryService) -> Vec<Result<Received, String>> {
        let id = self.group_id();
        let mut out = Vec::new();
        for stored in ds.after(&id, self.cursor) {
            self.cursor = stored.seq;
            if self.own.contains(&stored.seq) {
                continue;
            }
            let device = &self.device;
            out.push(device.receive(self.group.as_mut().unwrap(), &stored.bytes));
        }
        out
    }

    /// Joins from a Welcome after the checks a client makes before writing any
    /// state: the conversation binding and pin, and every member's credential.
    pub fn join_checked(
        &mut self,
        ds: &DeliveryService,
        welcome: &[u8],
        pins: &mut ConversationPins,
    ) -> Result<(), String> {
        let staged = self.device.stage_welcome(welcome)?;
        let binding = ConversationBinding::from_extensions(staged.group_context().extensions())
            .ok_or("welcome without a conversation binding")?;
        pins.check_welcome(&binding)?;
        for member in staged.members() {
            member_device(&member).map_err(|e| format!("member {}: {e}", member.index))?;
        }
        let group = staged
            .into_group(&self.device.provider)
            .map_err(|e| format!("{e:?}"))?;
        pins.pin(&binding);
        let id = group.group_id().clone();
        self.cursor = ds.after(&id, 0).last().map(|s| s.seq).unwrap_or(0);
        self.group = Some(group);
        Ok(())
    }

    /// Joins from a Welcome; the cursor starts at the commit that added us.
    pub fn join(&mut self, ds: &DeliveryService, welcome: &[u8]) {
        let group = self.device.join(welcome);
        let id = group.group_id().clone();
        self.cursor = ds.after(&id, 0).last().map(|s| s.seq).unwrap_or(0);
        self.group = Some(group);
    }

    pub fn texts(results: Vec<Result<Received, String>>) -> Vec<String> {
        results
            .into_iter()
            .filter_map(|r| match r {
                Ok(Received::Application { text, .. }) => Some(text),
                _ => None,
            })
            .collect()
    }
}
