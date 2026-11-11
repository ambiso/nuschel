// SPDX-License-Identifier: EUPL-1.2
//! A device's MLS state (docs/e2ee.md §6), for the crypto worker.
//!
//! The worker owns a [`Client`]: OpenMLS's records in memory, persisted by the
//! worker as a whole ([`Client::snapshot`]) in its sealed store. The device's leaf
//! signature key never enters OpenMLS storage; operations that sign take the
//! device's keys ([`nuschel_identity::DeviceKeys`]) and sign through them.
//!
//! Nothing that adds members is merged without the caller seeing every member
//! first: [`Client::stage_welcome`] and [`Client::process`] return the members of
//! the resulting group ([`MemberInfo`]: credential and leaf key), and only
//! [`Client::join`] and [`Client::merge`] apply them, so the worker checks each
//! credential against key transparency in between. Trees from the server and the
//! trees commits produce are checked for well-formed keys ([`crate::prevalidate`])
//! before anything else happens.
use crate::prevalidate::{self, ContentType, WireFormat};
use crate::provider::{
    Provider, binding_in, binding_of, capabilities, group_context_extensions, join_config,
    key_package_in, member_info, message_in, serialize, tree_in, wire_policy,
};
use crate::{CREDENTIAL_TYPE, ConversationBinding, Error, MAX_GROUP_DEVICES, SUITE, guard, mls};
use nuschel_identity::{DeviceCredential, DeviceKeys};
use openmls::prelude::*;
use openmls_traits::OpenMlsProvider;
use openmls_traits::signatures::{Signer, SignerError};

pub use crate::provider::MemberInfo;

/// Signs with the device's MLS leaf key (Ed25519), held by nuschel-identity.
struct LeafSigner<'a>(&'a DeviceKeys);

impl Signer for LeafSigner<'_> {
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, SignerError> {
        self.0
            .sign_leaf(payload)
            .map(|signature| signature.to_vec())
            .map_err(|_| SignerError::SigningError)
    }

    fn signature_scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

fn credential_with_key(device: &DeviceKeys, credential: &DeviceCredential) -> CredentialWithKey {
    CredentialWithKey {
        credential: Credential::new(CredentialType::Other(CREDENTIAL_TYPE), credential.encode()),
        signature_key: device.leaf_key().as_bytes().to_vec().into(),
    }
}

/// A key package that passed the structural checks and OpenMLS's validation; its
/// credential still needs the caller's check.
pub struct ValidKeyPackage {
    pub credential: Vec<u8>,
    pub signature_key: Vec<u8>,
    package: KeyPackage,
}

impl ValidKeyPackage {
    pub fn check(bytes: &[u8]) -> Result<Self, Error> {
        let facts = prevalidate::key_package(bytes)?;
        guard(|| {
            let provider = Provider::new()?;
            let package = key_package_in(bytes)?
                .validate(provider.crypto(), ProtocolVersion::Mls10)
                .map_err(mls)?;
            if package.ciphersuite() != SUITE {
                return Err(Error::Invalid("another ciphersuite"));
            }
            Ok(Self {
                credential: facts.leaf.credential.clone(),
                signature_key: facts.leaf.signature_key.to_vec(),
                package,
            })
        })
    }
}

/// What to send for a commit: the commit, the GroupInfo of the new epoch, and a
/// Welcome if members were added. The commit is pending until [`Client::merge_pending`]
/// (after the server's ack) or [`Client::clear_pending`] (after a refusal).
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub commit: Vec<u8>,
    pub group_info: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
    /// The epoch the commit leads to.
    pub epoch: u64,
}

/// A new group: its epoch-0 GroupInfo and tree (the server registers the group from
/// them) and the commit adding the first members.
#[derive(Clone, Debug)]
pub struct Created {
    pub group_id: Vec<u8>,
    pub init_group_info: Vec<u8>,
    pub init_tree: Vec<u8>,
    pub first: Outgoing,
}

/// A Welcome staged but not yet joined.
pub struct PendingJoin {
    pub binding: ConversationBinding,
    pub epoch: u64,
    pub members: Vec<MemberInfo>,
    pub own_index: u32,
    staged: StagedWelcome,
}

/// A commit from someone else, staged but not merged.
pub struct PendingCommit {
    /// The epoch the commit leads to.
    pub epoch: u64,
    /// Every member after the commit.
    pub members: Vec<MemberInfo>,
    /// This device is removed by it.
    pub self_removed: bool,
    staged: Box<StagedCommit>,
}

/// What an incoming handshake or application message is.
pub enum Incoming {
    Application { sender: MemberInfo, data: Vec<u8> },
    Commit(PendingCommit),
    Proposal,
}

pub struct Client {
    provider: Provider,
}

fn members_of_tree(bytes: &[u8]) -> Result<Vec<MemberInfo>, Error> {
    let leaves = prevalidate::ratchet_tree(bytes)?;
    if leaves.iter().flatten().count() > MAX_GROUP_DEVICES {
        return Err(Error::Invalid("too many members"));
    }
    Ok(leaves
        .into_iter()
        .enumerate()
        .filter_map(|(index, leaf)| {
            leaf.map(|leaf| MemberInfo {
                index: index as u32,
                credential: leaf.credential,
                signature_key: leaf.signature_key.to_vec(),
            })
        })
        .collect())
}

impl Client {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            provider: Provider::new()?,
        })
    }

    pub fn restore(snapshot: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            provider: Provider::restore(snapshot)?,
        })
    }

    /// Everything to persist (private keys included): seal it.
    pub fn snapshot(&self) -> Vec<u8> {
        self.provider.snapshot()
    }

    fn group(&self, group_id: &[u8]) -> Result<MlsGroup, Error> {
        MlsGroup::load(self.provider.storage(), &GroupId::from_slice(group_id))
            .map_err(mls)?
            .ok_or(Error::UnknownGroup)
    }

    pub fn has_group(&self, group_id: &[u8]) -> bool {
        self.group(group_id).is_ok()
    }

    /// A key package for the directory; its private keys stay in this state until a
    /// Welcome uses them (a last-resort package's stay for good).
    pub fn key_package(
        &self,
        device: &DeviceKeys,
        credential: &DeviceCredential,
        last_resort: bool,
    ) -> Result<Vec<u8>, Error> {
        guard(|| {
            let mut builder = KeyPackage::builder().leaf_node_capabilities(capabilities());
            if last_resort {
                builder = builder.mark_as_last_resort();
            }
            let bundle = builder
                .build(
                    SUITE,
                    &self.provider,
                    &LeafSigner(device),
                    credential_with_key(device, credential),
                )
                .map_err(mls)?;
            serialize(bundle.key_package())
        })
    }

    fn commit_messages(
        group: &mut MlsGroup,
        bundle: CommitMessageBundle,
    ) -> Result<Outgoing, Error> {
        let (commit, welcome, group_info) = bundle.into_messages();
        let group_info = group_info.ok_or(Error::Invalid("no GroupInfo for the commit"))?;
        Ok(Outgoing {
            commit: serialize(&commit)?,
            group_info: serialize(&group_info)?,
            welcome: welcome.map(|welcome| serialize(&welcome)).transpose()?,
            epoch: group
                .pending_commit()
                .map_or(group.epoch().as_u64() + 1, |staged| staged.epoch().as_u64()),
        })
    }

    /// Creates the group for `binding` and the commit adding `members` (their
    /// credentials checked by the caller). The commit is pending.
    pub fn create(
        &self,
        device: &DeviceKeys,
        credential: &DeviceCredential,
        binding: &ConversationBinding,
        members: &[ValidKeyPackage],
    ) -> Result<Created, Error> {
        guard(|| {
            let signer = LeafSigner(device);
            let mut group = MlsGroup::builder()
                .with_group_id(GroupId::from_slice(&binding.group_id()))
                .ciphersuite(SUITE)
                .with_capabilities(capabilities())
                .with_group_context_extensions(group_context_extensions(binding)?)
                .with_wire_format_policy(wire_policy())
                .use_ratchet_tree_extension(false)
                .max_past_epochs(0)
                .build(
                    &self.provider,
                    &signer,
                    credential_with_key(device, credential),
                )
                .map_err(mls)?;
            let init_group_info = serialize(
                &group
                    .export_group_info(self.provider.crypto(), &signer, false)
                    .map_err(mls)?,
            )?;
            let init_tree = serialize(&group.export_ratchet_tree())?;
            let first = self.add_to(&mut group, device, members)?;
            Ok(Created {
                group_id: binding.group_id(),
                init_group_info,
                init_tree,
                first,
            })
        })
    }

    fn add_to(
        &self,
        group: &mut MlsGroup,
        device: &DeviceKeys,
        members: &[ValidKeyPackage],
    ) -> Result<Outgoing, Error> {
        let bundle = group
            .commit_builder()
            .propose_adds(members.iter().map(|member| member.package.clone()))
            .load_psks(self.provider.storage())
            .map_err(mls)?
            .create_group_info(true)
            .build(
                self.provider.rand(),
                self.provider.crypto(),
                &LeafSigner(device),
                |_| true,
            )
            .map_err(mls)?
            .stage_commit(&self.provider)
            .map_err(mls)?;
        Self::commit_messages(group, bundle)
    }

    /// A commit adding devices to an existing group.
    pub fn add(
        &self,
        device: &DeviceKeys,
        group_id: &[u8],
        members: &[ValidKeyPackage],
    ) -> Result<Outgoing, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            self.add_to(&mut group, device, members)
        })
    }

    /// A commit removing leaves.
    pub fn remove(
        &self,
        device: &DeviceKeys,
        group_id: &[u8],
        leaves: &[u32],
    ) -> Result<Outgoing, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            let bundle = group
                .commit_builder()
                .propose_removals(leaves.iter().map(|&leaf| LeafNodeIndex::new(leaf)))
                .load_psks(self.provider.storage())
                .map_err(mls)?
                .create_group_info(true)
                .build(
                    self.provider.rand(),
                    self.provider.crypto(),
                    &LeafSigner(device),
                    |_| true,
                )
                .map_err(mls)?
                .stage_commit(&self.provider)
                .map_err(mls)?;
            Self::commit_messages(&mut group, bundle)
        })
    }

    /// A commit with a fresh path (post-compromise security, docs/e2ee.md §6.5).
    pub fn self_update(&self, device: &DeviceKeys, group_id: &[u8]) -> Result<Outgoing, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            let bundle = group
                .commit_builder()
                .force_self_update(true)
                .load_psks(self.provider.storage())
                .map_err(mls)?
                .create_group_info(true)
                .build(
                    self.provider.rand(),
                    self.provider.crypto(),
                    &LeafSigner(device),
                    |_| true,
                )
                .map_err(mls)?
                .stage_commit(&self.provider)
                .map_err(mls)?;
            Self::commit_messages(&mut group, bundle)
        })
    }

    pub fn has_pending_commit(&self, group_id: &[u8]) -> bool {
        self.group(group_id)
            .is_ok_and(|group| group.pending_commit().is_some())
    }

    /// The server accepted our commit: apply it. Returns the new epoch.
    pub fn merge_pending(&self, group_id: &[u8]) -> Result<u64, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            group.merge_pending_commit(&self.provider).map_err(mls)?;
            Ok(group.epoch().as_u64())
        })
    }

    /// The server refused our commit: forget it.
    pub fn clear_pending(&self, group_id: &[u8]) -> Result<(), Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            group
                .clear_pending_commit(self.provider.storage())
                .map_err(mls)
        })
    }

    /// Forgets a group entirely (a creation the server refused, or after removal).
    pub fn delete_group(&self, group_id: &[u8]) -> Result<(), Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            group.delete(self.provider.storage()).map_err(mls)
        })
    }

    /// Decrypts a Welcome with the tree the server serves (checked first) and stages
    /// it; nothing is joined until [`Self::join`]. The Welcome's key package is used
    /// up either way.
    pub fn stage_welcome(&self, welcome: &[u8], tree: &[u8]) -> Result<PendingJoin, Error> {
        if prevalidate::wire_format(welcome)? != WireFormat::Welcome {
            return Err(Error::Invalid("not a Welcome"));
        }
        let members = members_of_tree(tree)?;
        guard(|| {
            let MlsMessageBodyIn::Welcome(welcome) = message_in(welcome)?.extract() else {
                return Err(Error::Invalid("not a Welcome"));
            };
            if welcome.ciphersuite() != SUITE {
                return Err(Error::Invalid("a Welcome for another ciphersuite"));
            }
            let staged = StagedWelcome::new_from_welcome(
                &self.provider,
                &join_config(),
                welcome,
                Some(tree_in(tree)?),
            )
            .map_err(mls)?;
            let context = staged.group_context();
            if context.ciphersuite() != SUITE {
                return Err(Error::Invalid("another ciphersuite"));
            }
            let binding = binding_of(context).ok_or(Error::Invalid("no conversation binding"))?;
            if context.group_id().as_slice() != binding.group_id() {
                return Err(Error::Invalid("the group id does not match its binding"));
            }
            let staged_members = staged
                .members()
                .map(|member| member_info(&member))
                .collect::<Result<Vec<_>, _>>()?;
            if staged_members != members {
                return Err(Error::Invalid("the tree does not match the group"));
            }
            Ok(PendingJoin {
                binding,
                epoch: context.epoch().as_u64(),
                own_index: staged.own_leaf_index().u32(),
                members,
                staged,
            })
        })
    }

    /// Joins a staged Welcome whose members the caller accepted.
    pub fn join(&self, pending: PendingJoin) -> Result<Vec<u8>, Error> {
        guard(|| {
            let group = pending.staged.into_group(&self.provider).map_err(mls)?;
            Ok(group.group_id().as_slice().to_vec())
        })
    }

    /// Processes a handshake or application message from the stream. A commit is
    /// staged with the members it leads to, and applied only by [`Self::merge`].
    pub fn process(&self, group_id: &[u8], bytes: &[u8]) -> Result<Incoming, Error> {
        let facts = prevalidate::message(bytes)?;
        if facts.group_id != group_id {
            return Err(Error::Invalid("for another group"));
        }
        let handshake = facts.content_type != ContentType::Application;
        if handshake != (facts.wire_format == WireFormat::PublicMessage) {
            return Err(Error::Invalid(
                "handshakes must be public, application data private",
            ));
        }
        guard(|| {
            let mut group = self.group(group_id)?;
            let message = message_in(bytes)?
                .try_into_protocol_message()
                .map_err(mls)?;
            let processed = group
                .process_message(&self.provider, message)
                .map_err(mls)?;
            let sender = match processed.sender() {
                Sender::Member(index) => Some(*index),
                _ => None,
            };
            match processed.into_content() {
                ProcessedMessageContent::ApplicationMessage(application) => {
                    let index =
                        sender.ok_or(Error::Invalid("application data from a non-member"))?;
                    let member = group
                        .members()
                        .find(|member| member.index == index)
                        .ok_or(Error::Invalid("unknown sender"))?;
                    Ok(Incoming::Application {
                        sender: member_info(&member)?,
                        data: application.into_bytes(),
                    })
                }
                ProcessedMessageContent::StagedCommitMessage(staged) => {
                    let tree = staged
                        .export_ratchet_tree(self.provider.crypto(), group.export_ratchet_tree())
                        .map_err(mls)?
                        .ok_or(Error::Invalid("no tree for the commit"))?;
                    let members = members_of_tree(&serialize(&tree)?)?;
                    if binding_of(staged.group_context()) != binding_in(group.extensions()) {
                        return Err(Error::Invalid(
                            "the commit changes the conversation binding",
                        ));
                    }
                    Ok(Incoming::Commit(PendingCommit {
                        epoch: staged.epoch().as_u64(),
                        members,
                        self_removed: staged.self_removed(),
                        staged,
                    }))
                }
                ProcessedMessageContent::ProposalMessage(proposal) => {
                    group
                        .store_pending_proposal(self.provider.storage(), *proposal)
                        .map_err(mls)?;
                    Ok(Incoming::Proposal)
                }
                _ => Err(Error::Invalid("unexpected message")),
            }
        })
    }

    /// Applies a commit whose members the caller accepted. Returns the new epoch.
    pub fn merge(&self, group_id: &[u8], commit: PendingCommit) -> Result<u64, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            group
                .merge_staged_commit(&self.provider, *commit.staged)
                .map_err(mls)?;
            Ok(group.epoch().as_u64())
        })
    }

    /// An application message for the current epoch.
    pub fn encrypt(
        &self,
        device: &DeviceKeys,
        group_id: &[u8],
        data: &[u8],
    ) -> Result<Vec<u8>, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            let message = group
                .create_message(&self.provider, &LeafSigner(device), data)
                .map_err(mls)?;
            serialize(&message)
        })
    }

    pub fn epoch(&self, group_id: &[u8]) -> Result<u64, Error> {
        Ok(self.group(group_id)?.epoch().as_u64())
    }

    pub fn is_active(&self, group_id: &[u8]) -> bool {
        self.group(group_id).is_ok_and(|group| group.is_active())
    }

    pub fn own_index(&self, group_id: &[u8]) -> Result<u32, Error> {
        Ok(self.group(group_id)?.own_leaf_index().u32())
    }

    pub fn members(&self, group_id: &[u8]) -> Result<Vec<MemberInfo>, Error> {
        self.group(group_id)?
            .members()
            .map(|member| member_info(&member))
            .collect()
    }

    pub fn binding(&self, group_id: &[u8]) -> Result<ConversationBinding, Error> {
        binding_in(self.group(group_id)?.extensions())
            .ok_or(Error::Invalid("no conversation binding"))
    }

    /// The epoch authenticator (RFC 9420 §8.7): equal for all members of an epoch.
    pub fn epoch_authenticator(&self, group_id: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self
            .group(group_id)?
            .epoch_authenticator()
            .as_slice()
            .to_vec())
    }
}
