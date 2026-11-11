// SPDX-License-Identifier: EUPL-1.2
//! The delivery service's view of a group (docs/e2ee.md §6.3), for `nuscheld`.
//!
//! The server is not a member: it follows each group's public state with OpenMLS's
//! `PublicGroup` from the handshake messages, which travel as PublicMessage. With
//! that it
//!
//! - accepts exactly one commit per epoch and refuses the rest with the current
//!   epoch ([`DsError::StaleEpoch`]);
//! - checks framing, signatures against the sender's leaf and the proposal rules
//!   before fan-out (membership and confirmation tags need epoch secrets, which only
//!   members check);
//! - after a commit, checks every key in the new tree ([`crate::prevalidate`]) and
//!   that the uploaded GroupInfo is signed by a member and names exactly the
//!   server's new group context; a failure rolls the group back;
//! - checks that a Welcome goes to exactly the key packages the commit adds;
//! - accepts application messages only for the current epoch ([`DeliveryGroup::fence`]).
//!
//! A group's state is [`DeliveryGroup::state`]: OpenMLS's own records for the
//! public group and the newest GroupInfo. It is restored without validating again
//! (leaf lifetimes would expire), so it must only ever come from this module.
use crate::prevalidate::{self, ContentType, WireFormat};
use crate::provider::{
    MemberInfo, Provider, binding_of, key_package_in, member_info, message_in, serialize, tree_in,
};
use crate::{ConversationBinding, Error, SUITE, guard, mls};
use openmls::messages::group_info::VerifiableGroupInfo;
use openmls::prelude::*;
use openmls_traits::OpenMlsProvider;

/// Why the delivery service refused a handshake or application message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DsError {
    /// For another epoch than the current one: fetch the stream, process the winning
    /// commit, retry (with the same idempotency key).
    #[error("stale epoch (the group is at epoch {current})")]
    StaleEpoch { current: u64 },
    #[error(transparent)]
    Invalid(#[from] Error),
}

/// What a valid key package carries, for the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageInfo {
    /// The Nuschel device credential (nuschel-identity `DeviceCredential`).
    pub credential: Vec<u8>,
    pub signature_key: [u8; 32],
    /// `KeyPackageRef` (RFC 9420 §5.2): how a Welcome names its recipient.
    pub reference: Vec<u8>,
    pub last_resort: bool,
    /// The leaf's lifetime (Unix seconds).
    pub not_after: u64,
}

/// Checks a key package for the directory: structure and key lengths first, then
/// OpenMLS's validation (signatures, lifetime, init key differs from the leaf key).
pub fn check_key_package(bytes: &[u8]) -> Result<KeyPackageInfo, Error> {
    let facts = prevalidate::key_package(bytes)?;
    guard(|| {
        let provider = Provider::new()?;
        let package = key_package_in(bytes)?
            .validate(provider.crypto(), ProtocolVersion::Mls10)
            .map_err(mls)?;
        if package.ciphersuite() != SUITE {
            return Err(Error::Invalid("another ciphersuite"));
        }
        let reference = package.hash_ref(provider.crypto()).map_err(mls)?;
        Ok(KeyPackageInfo {
            credential: facts.leaf.credential.clone(),
            signature_key: facts.leaf.signature_key,
            reference: reference.as_slice().to_vec(),
            last_resort: facts.last_resort,
            not_after: facts.leaf.lifetime.map_or(0, |(_, not_after)| not_after),
        })
    })
}

/// What an accepted commit changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The epoch the group is in now.
    pub epoch: u64,
    /// Every member after the commit.
    pub members: Vec<MemberInfo>,
    /// Leaves added by the commit.
    pub added: Vec<MemberInfo>,
    /// The `KeyPackageRef`s the Welcome is for (those of the added key packages).
    pub welcome_for: Vec<Vec<u8>>,
}

pub struct DeliveryGroup {
    provider: Provider,
    public: PublicGroup,
    binding: ConversationBinding,
    group_info: Vec<u8>,
}

fn verifiable_group_info(bytes: &[u8]) -> Result<VerifiableGroupInfo, Error> {
    if prevalidate::wire_format(bytes)? != WireFormat::GroupInfo {
        return Err(Error::Invalid("not a GroupInfo"));
    }
    match message_in(bytes)?.extract() {
        MlsMessageBodyIn::GroupInfo(info) => Ok(info),
        _ => Err(Error::Invalid("not a GroupInfo")),
    }
}

/// The leaf that signed a GroupInfo: the last field of its to-be-signed part.
fn group_info_signer(info: &VerifiableGroupInfo) -> Result<LeafNodeIndex, Error> {
    let payload = info.unsigned_payload().map_err(mls)?;
    let tail = payload
        .len()
        .checked_sub(4)
        .and_then(|start| payload.get(start..))
        .ok_or(Error::Invalid("GroupInfo too short"))?;
    Ok(LeafNodeIndex::new(u32::from_be_bytes([
        tail[0], tail[1], tail[2], tail[3],
    ])))
}

impl DeliveryGroup {
    /// Registers a new group from its creator's signed GroupInfo for epoch 0 and the
    /// ratchet tree. The binding must name `server` and `conversation` at
    /// generation 0; the caller checks the members' credentials (returned).
    pub fn create(
        group_info: &[u8],
        tree: &[u8],
        server: &[u8; 32],
        conversation: &[u8; 16],
    ) -> Result<(Self, Vec<MemberInfo>), Error> {
        prevalidate::ratchet_tree(tree)?;
        guard(|| {
            let provider = Provider::new()?;
            let info = verifiable_group_info(group_info)?;
            if info.epoch().as_u64() != 0 {
                return Err(Error::Invalid("a new group starts at epoch 0"));
            }
            let (public, _) = PublicGroup::from_external(
                provider.crypto(),
                provider.storage(),
                tree_in(tree)?,
                info,
                ProposalStore::new(),
            )
            .map_err(mls)?;
            let binding = binding_of(public.group_context())
                .ok_or(Error::Invalid("no conversation binding"))?;
            if public.ciphersuite() != SUITE
                || binding.server != *server
                || binding.conversation != *conversation
                || binding.generation != 0
                || public.group_id().as_slice() != binding.group_id()
            {
                return Err(Error::Invalid("the binding names another conversation"));
            }
            if public.group_context().extensions().ratchet_tree().is_some() {
                return Err(Error::Invalid("a ratchet tree in the group context"));
            }
            let group = Self {
                provider,
                public,
                binding,
                group_info: group_info.to_vec(),
            };
            let members = group.members()?;
            Ok((group, members))
        })
    }

    /// The persistent state: OpenMLS's records, then the newest GroupInfo.
    pub fn state(&self) -> Vec<u8> {
        let snapshot = self.provider.snapshot();
        let mut out = Vec::with_capacity(4 + snapshot.len() + self.group_info.len());
        out.extend_from_slice(&(snapshot.len() as u32).to_be_bytes());
        out.extend_from_slice(&snapshot);
        out.extend_from_slice(&self.group_info);
        out
    }

    /// From [`Self::state`] of the same group.
    pub fn restore(group_id: &[u8], state: &[u8]) -> Result<Self, Error> {
        let corrupt = || Error::Storage("corrupt group state".into());
        let length = state.get(..4).ok_or_else(corrupt)?;
        let length = u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize;
        let snapshot = state.get(4..4 + length).ok_or_else(corrupt)?;
        let group_info = state.get(4 + length..).ok_or_else(corrupt)?.to_vec();
        guard(|| {
            let provider = Provider::restore(snapshot)?;
            let public = PublicGroup::load(provider.storage(), &GroupId::from_slice(group_id))
                .map_err(mls)?
                .ok_or_else(corrupt)?;
            let binding = binding_of(public.group_context()).ok_or_else(corrupt)?;
            Ok(Self {
                provider,
                public,
                binding,
                group_info,
            })
        })
    }

    pub fn epoch(&self) -> u64 {
        self.public.group_context().epoch().as_u64()
    }

    pub fn group_id(&self) -> &[u8] {
        self.public.group_id().as_slice()
    }

    pub fn binding(&self) -> ConversationBinding {
        self.binding
    }

    pub fn members(&self) -> Result<Vec<MemberInfo>, Error> {
        self.public.members().map(|m| member_info(&m)).collect()
    }

    /// The newest GroupInfo and the server's tree: what a Welcome recipient (or an
    /// external joiner) needs besides its Welcome.
    pub fn join_info(&self) -> Result<(Vec<u8>, Vec<u8>), Error> {
        Ok((
            self.group_info.clone(),
            serialize(&self.public.export_ratchet_tree())?,
        ))
    }

    /// Application messages: a PrivateMessage of this group for the current epoch.
    pub fn fence(&self, bytes: &[u8]) -> Result<(), DsError> {
        let facts = prevalidate::message(bytes).map_err(Error::from)?;
        if facts.wire_format != WireFormat::PrivateMessage
            || facts.content_type != ContentType::Application
        {
            return Err(Error::Invalid("not an application message").into());
        }
        if facts.group_id != self.group_id() {
            return Err(Error::Invalid("for another group").into());
        }
        if facts.epoch != self.epoch() {
            return Err(DsError::StaleEpoch {
                current: self.epoch(),
            });
        }
        Ok(())
    }

    fn handshake(&self, bytes: &[u8]) -> Result<ContentType, DsError> {
        let facts = prevalidate::message(bytes).map_err(Error::from)?;
        if facts.wire_format != WireFormat::PublicMessage {
            return Err(Error::Invalid("handshake messages must be PublicMessage").into());
        }
        if facts.group_id != self.group_id() {
            return Err(Error::Invalid("for another group").into());
        }
        if facts.epoch != self.epoch() {
            return Err(DsError::StaleEpoch {
                current: self.epoch(),
            });
        }
        Ok(facts.content_type)
    }

    /// A standalone proposal (by a member or the server as external sender): checked
    /// and queued for the next commit.
    pub fn proposal(&mut self, bytes: &[u8]) -> Result<(), DsError> {
        if self.handshake(bytes)? != ContentType::Proposal {
            return Err(Error::Invalid("not a proposal").into());
        }
        let before = self.provider.snapshot();
        let result = guard(|| {
            let message = message_in(bytes)?
                .try_into_protocol_message()
                .map_err(mls)?;
            let processed = self
                .public
                .process_message(self.provider.crypto(), message)
                .map_err(mls)?;
            match processed.into_content() {
                ProcessedMessageContent::ProposalMessage(proposal) => self
                    .public
                    .add_proposal(self.provider.storage(), *proposal)
                    .map_err(mls),
                _ => Err(Error::Invalid("not a proposal")),
            }
        });
        if result.is_err() {
            self.roll_back(&before);
        }
        result.map_err(DsError::from)
    }

    fn roll_back(&mut self, snapshot: &[u8]) {
        if let Ok(provider) = Provider::restore(snapshot) {
            self.provider.replace_with(provider);
        }
        if let Ok(Some(public)) = PublicGroup::load(self.provider.storage(), self.public.group_id())
        {
            self.public = public;
        }
    }

    /// A commit with the committer's GroupInfo for the new epoch and, if the commit
    /// adds members, the Welcome for them. On any failure the group is unchanged.
    pub fn commit(
        &mut self,
        commit: &[u8],
        group_info: &[u8],
        welcome: Option<&[u8]>,
    ) -> Result<CommitOutcome, DsError> {
        if self.handshake(commit)? != ContentType::Commit {
            return Err(Error::Invalid("not a commit").into());
        }
        let before = self.provider.snapshot();
        let result = guard(|| self.apply_commit(commit, group_info, welcome));
        match result {
            Ok(outcome) => {
                self.group_info = group_info.to_vec();
                Ok(outcome)
            }
            Err(error) => {
                self.roll_back(&before);
                Err(error.into())
            }
        }
    }

    fn apply_commit(
        &mut self,
        commit: &[u8],
        group_info: &[u8],
        welcome: Option<&[u8]>,
    ) -> Result<CommitOutcome, Error> {
        let message = message_in(commit)?
            .try_into_protocol_message()
            .map_err(mls)?;
        let processed = self
            .public
            .process_message(self.provider.crypto(), message)
            .map_err(mls)?;
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(Error::Invalid("not a commit"));
        };
        let mut welcome_for = Vec::new();
        for add in staged.add_proposals() {
            let reference = add
                .add_proposal()
                .key_package()
                .hash_ref(self.provider.crypto())
                .map_err(mls)?;
            welcome_for.push(reference.as_slice().to_vec());
        }
        let before: Vec<MemberInfo> = self.members()?;
        self.public
            .merge_commit(self.provider.storage(), *staged)
            .map_err(mls)?;
        // Every key a member could later encrypt to, and every leaf's credential type.
        let tree = serialize(&self.public.export_ratchet_tree())?;
        prevalidate::ratchet_tree(&tree)?;
        if binding_of(self.public.group_context()) != Some(self.binding) {
            return Err(Error::Invalid(
                "the commit changed the conversation binding",
            ));
        }
        self.check_group_info(group_info)?;
        self.check_welcome(welcome, &welcome_for)?;
        let members = self.members()?;
        let added = members
            .iter()
            .filter(|member| {
                !before.iter().any(|old| {
                    old.index == member.index && old.signature_key == member.signature_key
                })
            })
            .cloned()
            .collect();
        Ok(CommitOutcome {
            epoch: self.epoch(),
            members,
            added,
            welcome_for,
        })
    }

    /// The committer's GroupInfo: for the server's new group context exactly, signed
    /// by a member, without a ratchet tree (joiners get the server's), with a
    /// well-formed external public key if it has one.
    fn check_group_info(&self, bytes: &[u8]) -> Result<(), Error> {
        let info = verifiable_group_info(bytes)?;
        if info.group_context() != self.public.group_context() {
            return Err(Error::Invalid("the GroupInfo names another group state"));
        }
        if info.extensions().ratchet_tree().is_some() {
            return Err(Error::Invalid("a ratchet tree in the GroupInfo"));
        }
        if let Some(external) = info.extensions().external_pub()
            && external.external_pub().as_slice().len() != crate::SUITE_KEM_PUBLIC_KEY_LEN
        {
            return Err(Error::Invalid("malformed external public key"));
        }
        let signer = group_info_signer(&info)?;
        let leaf = self
            .public
            .leaf(signer)
            .ok_or(Error::Invalid("the GroupInfo's signer is not a member"))?;
        let key = OpenMlsSignaturePublicKey::from_signature_key(
            leaf.signature_key().clone(),
            SUITE.signature_algorithm(),
        );
        info.verify_no_out(self.provider.crypto(), &key)
            .map_err(|_| Error::Invalid("the GroupInfo's signature"))
    }

    fn check_welcome(&self, welcome: Option<&[u8]>, expected: &[Vec<u8>]) -> Result<(), Error> {
        let Some(bytes) = welcome else {
            return if expected.is_empty() {
                Ok(())
            } else {
                Err(Error::Invalid("members added without a Welcome"))
            };
        };
        if prevalidate::wire_format(bytes)? != WireFormat::Welcome {
            return Err(Error::Invalid("not a Welcome"));
        }
        let MlsMessageBodyIn::Welcome(welcome) = message_in(bytes)?.extract() else {
            return Err(Error::Invalid("not a Welcome"));
        };
        if welcome.ciphersuite() != SUITE {
            return Err(Error::Invalid("a Welcome for another ciphersuite"));
        }
        let mut named: Vec<Vec<u8>> = welcome
            .secrets()
            .iter()
            .map(|secret| secret.new_member().as_slice().to_vec())
            .collect();
        let mut expected = expected.to_vec();
        named.sort();
        expected.sort();
        if named != expected {
            return Err(Error::Invalid(
                "the Welcome is not for exactly the added key packages",
            ));
        }
        Ok(())
    }
}

/// Recipients of a Welcome, by `KeyPackageRef`, without checking anything else
/// (for routing an already accepted Welcome).
pub fn welcome_recipients(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    if prevalidate::wire_format(bytes)? != WireFormat::Welcome {
        return Err(Error::Invalid("not a Welcome"));
    }
    guard(|| {
        let MlsMessageBodyIn::Welcome(welcome) = message_in(bytes)?.extract() else {
            return Err(Error::Invalid("not a Welcome"));
        };
        Ok(welcome
            .secrets()
            .iter()
            .map(|secret| secret.new_member().as_slice().to_vec())
            .collect())
    })
}

pub use crate::provider::MemberInfo as Member;
