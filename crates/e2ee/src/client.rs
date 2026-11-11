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
    key_package_in, member_info, message_in, serialize, server_is_sole_sender, tree_in,
    wire_policy,
};
use crate::{CREDENTIAL_TYPE, ConversationBinding, Error, SUITE, guard, mls};
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
    /// Members the commit removes (as they were before it).
    pub removed: Vec<MemberInfo>,
    /// This device is removed by it.
    pub self_removed: bool,
    /// An external commit: a device joined by itself (docs/e2ee.md §6.2).
    pub external_join: bool,
    /// Add proposals in it (members adding devices; channels have none).
    pub adds: usize,
    /// Every removal in it was proposed by the server (the external sender), or is
    /// an external joiner's resync of its own old leaf.
    pub removals_by_server: bool,
    staged: Box<StagedCommit>,
}

/// What an incoming handshake or application message is.
pub enum Incoming {
    Application {
        sender: MemberInfo,
        data: Vec<u8>,
    },
    Commit(PendingCommit),
    /// A proposal, kept for the next commit. The server's are removals only:
    /// `removes` is the leaf it asks to remove.
    Proposal {
        from_server: bool,
        removes: Option<u32>,
    },
}

/// An external join (docs/e2ee.md §6.2): the group as this device will see it, and
/// the commit to send. OpenMLS merges an external commit as it makes it, so a
/// refused join is undone by deleting the group ([`Client::delete_group`]).
pub struct ExternalJoin {
    pub binding: ConversationBinding,
    /// Every member after the join, this device included.
    pub members: Vec<MemberInfo>,
    pub own_index: u32,
    pub outgoing: Outgoing,
}

pub struct Client {
    provider: Provider,
}

fn members_of_tree(bytes: &[u8]) -> Result<Vec<MemberInfo>, Error> {
    Ok(prevalidate::ratchet_tree(bytes)?
        .present
        .into_iter()
        .map(|(index, leaf)| MemberInfo {
            index,
            credential: leaf.credential,
            signature_key: leaf.signature_key.to_vec(),
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
    /// credentials checked by the caller); without members (a channel's first
    /// device) the commit is a self-update, so the group's first GroupInfo for
    /// external joiners comes from a commit. `server_key` is the server's
    /// key-transparency signing key: its only external sender. The commit is
    /// pending.
    pub fn create(
        &self,
        device: &DeviceKeys,
        credential: &DeviceCredential,
        binding: &ConversationBinding,
        server_key: &[u8; 32],
        members: &[ValidKeyPackage],
    ) -> Result<Created, Error> {
        guard(|| {
            let signer = LeafSigner(device);
            let mut group = MlsGroup::builder()
                .with_group_id(GroupId::from_slice(&binding.group_id()))
                .ciphersuite(SUITE)
                .with_capabilities(capabilities())
                .with_group_context_extensions(group_context_extensions(binding, server_key)?)
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
            .force_self_update(members.is_empty())
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

    /// Joins a group by external commit (docs/e2ee.md §6.2, a channel's lazy join)
    /// from the server's newest GroupInfo and tree (checked first). The group must
    /// name `server_key` as its only external sender. Nothing is sent yet; the
    /// caller checks the binding and every member, then sends the commit, and
    /// deletes the group if the server refuses it.
    pub fn join_external(
        &self,
        device: &DeviceKeys,
        credential: &DeviceCredential,
        group_info: &[u8],
        tree: &[u8],
        server_key: &[u8; 32],
    ) -> Result<ExternalJoin, Error> {
        if prevalidate::wire_format(group_info)? != WireFormat::GroupInfo {
            return Err(Error::Invalid("not a GroupInfo"));
        }
        let before = members_of_tree(tree)?;
        guard(|| {
            let MlsMessageBodyIn::GroupInfo(info) = message_in(group_info)?.extract() else {
                return Err(Error::Invalid("not a GroupInfo"));
            };
            if info.ciphersuite() != SUITE {
                return Err(Error::Invalid("a group of another ciphersuite"));
            }
            if info.extensions().ratchet_tree().is_some() {
                return Err(Error::Invalid("a ratchet tree in the GroupInfo"));
            }
            let (mut group, bundle) = MlsGroup::external_commit_builder()
                .with_config(join_config())
                .with_ratchet_tree(tree_in(tree)?)
                .build_group(
                    &self.provider,
                    info,
                    credential_with_key(device, credential),
                )
                .map_err(mls)?
                // The joiner's new leaf supports what every Nuschel leaf does.
                .leaf_node_parameters(
                    LeafNodeParameters::builder()
                        .with_capabilities(capabilities())
                        .build(),
                )
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
                .finalize(&self.provider)
                .map_err(mls)?;
            let checked = (|| {
                let binding = binding_in(group.extensions())
                    .ok_or(Error::Invalid("no conversation binding"))?;
                if group.group_id().as_slice() != binding.group_id() {
                    return Err(Error::Invalid("the group id does not match its binding"));
                }
                if !server_is_sole_sender(group.extensions(), server_key) {
                    return Err(Error::Invalid(
                        "the group's external sender is not this server",
                    ));
                }
                let members = group
                    .members()
                    .map(|member| member_info(&member))
                    .collect::<Result<Vec<_>, _>>()?;
                let own_index = group.own_leaf_index().u32();
                // The server's tree, and nothing else, plus this device.
                if members
                    .iter()
                    .any(|member| member.index != own_index && !before.contains(member))
                {
                    return Err(Error::Invalid("the tree does not match the group"));
                }
                let (commit, _, group_info) = bundle.into_messages();
                let group_info = group_info.ok_or(Error::Invalid("no GroupInfo for the commit"))?;
                Ok(ExternalJoin {
                    binding,
                    members,
                    own_index,
                    outgoing: Outgoing {
                        commit: serialize(&commit)?,
                        group_info: serialize(&group_info)?,
                        welcome: None,
                        epoch: group.epoch().as_u64(),
                    },
                })
            })();
            if checked.is_err() {
                let _ = group.delete(self.provider.storage());
            }
            checked
        })
    }

    /// The leaves the server's pending proposals remove (docs/e2ee.md §6.2).
    pub fn pending_removals(&self, group_id: &[u8]) -> Result<Vec<u32>, Error> {
        let group = self.group(group_id)?;
        Ok(group
            .pending_proposals()
            .filter(|queued| matches!(queued.sender(), Sender::External(_)))
            .filter_map(|queued| match queued.proposal() {
                Proposal::Remove(remove) => Some(remove.removed().u32()),
                _ => None,
            })
            .collect())
    }

    /// A commit of the pending proposals: the server's removals (it asked this
    /// device to commit them). Refused when there are none, or anything else is
    /// pending, or one would remove this device (another member must).
    pub fn commit_proposals(
        &self,
        device: &DeviceKeys,
        group_id: &[u8],
    ) -> Result<Outgoing, Error> {
        guard(|| {
            let mut group = self.group(group_id)?;
            let own = group.own_leaf_index();
            let mut any = false;
            for queued in group.pending_proposals() {
                let Proposal::Remove(remove) = queued.proposal() else {
                    return Err(Error::Invalid("a pending proposal that is not a removal"));
                };
                if !matches!(queued.sender(), Sender::External(_)) {
                    return Err(Error::Invalid(
                        "a pending removal the server did not propose",
                    ));
                }
                if remove.removed() == own {
                    return Err(Error::Invalid("the server asks to remove this device"));
                }
                any = true;
            }
            if !any {
                return Err(Error::Invalid("nothing to commit"));
            }
            let bundle = group
                .commit_builder()
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
            let external_join = matches!(processed.sender(), Sender::NewMemberCommit);
            let from_server = matches!(processed.sender(), Sender::External(_));
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
                        .map_err(mls)?;
                    // A commit removing this device stages without member state: there
                    // is no tree for us after it, and no member to check.
                    let members = match tree {
                        Some(tree) => members_of_tree(&serialize(&tree)?)?,
                        None if staged.self_removed() => Vec::new(),
                        None => return Err(Error::Invalid("no tree for the commit")),
                    };
                    if binding_of(staged.group_context()) != binding_in(group.extensions()) {
                        return Err(Error::Invalid(
                            "the commit changes the conversation binding",
                        ));
                    }
                    if staged.group_context().extensions().external_senders()
                        != group.extensions().external_senders()
                    {
                        return Err(Error::Invalid("the commit changes the external sender"));
                    }
                    let before: Vec<MemberInfo> = group
                        .members()
                        .map(|member| member_info(&member))
                        .collect::<Result<_, _>>()?;
                    let removed_leaves: Vec<u32> = staged
                        .remove_proposals()
                        .map(|remove| remove.remove_proposal().removed().u32())
                        .collect();
                    let removals_by_server = staged.remove_proposals().all(|remove| {
                        matches!(
                            remove.sender(),
                            Sender::External(_) | Sender::NewMemberCommit
                        )
                    });
                    let removed = before
                        .into_iter()
                        .filter(|member| removed_leaves.contains(&member.index))
                        .collect();
                    Ok(Incoming::Commit(PendingCommit {
                        epoch: staged.epoch().as_u64(),
                        members,
                        removed,
                        self_removed: staged.self_removed(),
                        external_join,
                        adds: staged.add_proposals().count(),
                        removals_by_server,
                        staged,
                    }))
                }
                ProcessedMessageContent::ProposalMessage(proposal) => {
                    // The server proposes removals only (docs/e2ee.md §6.2).
                    let removes = match proposal.proposal() {
                        Proposal::Remove(remove) => Some(remove.removed().u32()),
                        _ => None,
                    };
                    if from_server && removes.is_none() {
                        return Err(Error::Invalid("the server may only propose removals"));
                    }
                    group
                        .store_pending_proposal(self.provider.storage(), *proposal)
                        .map_err(mls)?;
                    Ok(Incoming::Proposal {
                        from_server,
                        removes,
                    })
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

    /// Export only a domain-separated archive application key, never the MLS
    /// exporter secret. Its index is this group's epoch; retain only this key.
    pub fn history_key(&self, group_id: &[u8]) -> Result<crate::history::HistoryKey, Error> {
        let group = self.group(group_id)?;
        let binding = self.binding(group_id)?;
        let index = u32::try_from(group.epoch().as_u64())
            .map_err(|_| Error::Invalid("history key epoch exceeds its wire index"))?;
        let key = group
            .export_secret(
                self.provider.crypto(),
                "nuschel/archive-key/v1",
                &binding.encode(),
                32,
            )
            .map_err(mls)?;
        Ok(crate::history::HistoryKey {
            index,
            key: key
                .try_into()
                .map_err(|_| Error::Invalid("archive key length"))?,
        })
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
