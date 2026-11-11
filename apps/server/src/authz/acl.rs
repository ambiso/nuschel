// SPDX-License-Identifier: EUPL-1.2
//! Mumble's permission model: per-channel ACL entries inherited down the tree,
//! groups (with inheritance and add/remove lists) and the meta groups `all`,
//! `auth`, `strong`, `in`, `out`, `sub,a,b,c`, `#token`, `$hash`, with the `!`
//! (invert) and `~` (evaluate in the ACL's channel) prefixes. Behaviour follows
//! stock Murmur's `ChanACL::effectivePermissions` and `Group::appliesToUser`.
//!
//! The evaluation functions are private to [`crate::authz`]: handlers ask it
//! whether an actor may do something and never evaluate ACLs themselves. The
//! data types and the editor's views (inherited entries, group members) are
//! public.
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub mod perm {
    pub const NONE: u32 = 0;
    pub const WRITE: u32 = 0x1;
    pub const TRAVERSE: u32 = 0x2;
    pub const ENTER: u32 = 0x4;
    pub const SPEAK: u32 = 0x8;
    pub const MUTE_DEAFEN: u32 = 0x10;
    pub const MOVE: u32 = 0x20;
    pub const MAKE_CHANNEL: u32 = 0x40;
    pub const LINK_CHANNEL: u32 = 0x80;
    pub const WHISPER: u32 = 0x100;
    pub const TEXT_MESSAGE: u32 = 0x200;
    pub const MAKE_TEMP_CHANNEL: u32 = 0x400;
    pub const LISTEN: u32 = 0x800;
    pub const KICK: u32 = 0x10000;
    pub const BAN: u32 = 0x20000;
    pub const REGISTER: u32 = 0x40000;
    pub const SELF_REGISTER: u32 = 0x80000;
    pub const RESET_USER_CONTENT: u32 = 0x100000;
    /// Only meaningful in a cache; never stored or sent.
    pub const CACHED: u32 = 0x8000000;
    pub const ALL: u32 = WRITE
        | TRAVERSE
        | ENTER
        | SPEAK
        | MUTE_DEAFEN
        | MOVE
        | MAKE_CHANNEL
        | LINK_CHANNEL
        | WHISPER
        | TEXT_MESSAGE
        | MAKE_TEMP_CHANNEL
        | LISTEN
        | KICK
        | BAN
        | REGISTER
        | SELF_REGISTER
        | RESET_USER_CONTENT;
    /// Granted to everyone before any ACL applies.
    pub const DEFAULT: u32 = TRAVERSE | ENTER | SPEAK | WHISPER | TEXT_MESSAGE | LISTEN;
    /// Server-wide permissions that only the root channel's own ACLs can grant.
    pub const ROOT_ONLY: u32 = KICK | BAN | RESET_USER_CONTENT | REGISTER | SELF_REGISTER;

    pub fn name(permission: u32) -> &'static str {
        match permission {
            WRITE => "Write ACL",
            TRAVERSE => "Traverse",
            ENTER => "Enter",
            SPEAK => "Speak",
            MUTE_DEAFEN => "Mute/Deafen",
            MOVE => "Move",
            MAKE_CHANNEL => "Make channel",
            LINK_CHANNEL => "Link channel",
            WHISPER => "Whisper",
            TEXT_MESSAGE => "Text message",
            MAKE_TEMP_CHANNEL => "Make temporary",
            LISTEN => "Listen",
            KICK => "Kick",
            BAN => "Ban",
            REGISTER => "Register User",
            SELF_REGISTER => "Register Self",
            RESET_USER_CONTENT => "Reset User Content",
            _ => "Unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AclEntry {
    pub apply_here: bool,
    pub apply_subs: bool,
    /// Either a registered user or a group specification.
    pub user_id: Option<u32>,
    pub group: String,
    pub grant: u32,
    pub deny: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Group {
    pub inherit: bool,
    pub inheritable: bool,
    pub add: BTreeSet<u32>,
    pub remove: BTreeSet<u32>,
}

impl Group {
    pub fn new() -> Self {
        Self {
            inherit: true,
            inheritable: true,
            ..Default::default()
        }
    }
}

/// The parts of a channel the permission model reads.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub parent: Option<u32>,
    pub inherit_acl: bool,
    pub acls: Vec<AclEntry>,
    pub groups: BTreeMap<String, Group>,
}

pub trait Tree {
    fn node(&self, channel: u32) -> Option<&Node>;
}

impl Tree for HashMap<u32, Node> {
    fn node(&self, channel: u32) -> Option<&Node> {
        self.get(&channel)
    }
}

/// Who is asking: the facts groups can match on.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    pub user_id: Option<u32>,
    /// The channel the user is in.
    pub channel: u32,
    pub hash: Option<&'a str>,
    /// SHA-256 digests of the access tokens, lowercased first (Mumble compares
    /// tokens case-insensitively); see [`token_digest`].
    pub tokens: &'a [[u8; 32]],
    /// A certificate that chains to a configured certificate authority
    /// (Murmur's `bVerified`, the `strong` group; see `tls::Strength`).
    pub strong: bool,
}

impl Subject<'_> {
    fn superuser(&self) -> bool {
        self.user_id == Some(0)
    }
}

/// Access tokens are channel passwords: they are kept and compared only as
/// digests, in constant time.
pub fn token_digest(token: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.to_lowercase().as_bytes());
    let mut out = [0; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Channels from the root down to `channel`, inclusive; empty if it does not exist.
/// Bounded by the tree's depth; a cycle (never created by the server) ends the walk.
fn path_from_root(tree: &impl Tree, channel: u32) -> Vec<u32> {
    let mut path = Vec::new();
    let mut current = Some(channel);
    while let Some(id) = current {
        let Some(node) = tree.node(id) else { break };
        if path.len() > 4096 || path.contains(&id) {
            break;
        }
        path.push(id);
        current = node.parent;
    }
    path.reverse();
    path
}

pub(super) fn effective_permissions(tree: &impl Tree, subject: &Subject, channel: u32) -> u32 {
    use perm::*;
    if subject.superuser() {
        return ALL & !(SPEAK | WHISPER);
    }
    let path = path_from_root(tree, channel);
    let mut granted = DEFAULT;
    let mut traverse = true;
    let mut write = false;
    for &current in &path {
        let Some(node) = tree.node(current) else {
            break;
        };
        if !node.inherit_acl {
            granted = DEFAULT;
        }
        for acl in &node.acls {
            let matches_user = acl.user_id.is_some() && acl.user_id == subject.user_id;
            let matches_group =
                acl.user_id.is_none() && group_applies(tree, channel, current, &acl.group, subject);
            if !(matches_user || matches_group) {
                continue;
            }
            let here = current == channel && acl.apply_here;
            let inherited = current != channel && acl.apply_subs;
            let apply = here || inherited;
            // Traverse is denied on the way down even by an ACL that applies only
            // to its own channel: a user who cannot pass a parent cannot reach C.
            if inherited || acl.apply_here {
                if acl.grant & TRAVERSE != 0 {
                    traverse = true;
                }
                if acl.deny & TRAVERSE != 0 {
                    traverse = false;
                }
            }
            if apply {
                if acl.grant & WRITE != 0 {
                    write = true;
                }
                if acl.deny & WRITE != 0 {
                    write = false;
                }
            }
            if current == 0 && here {
                granted |= acl.grant & ROOT_ONLY;
            }
            if apply {
                granted |= acl.grant & !(ROOT_ONLY | CACHED);
                granted &= !acl.deny;
            }
        }
        if !traverse && !write {
            return NONE;
        }
    }
    if path.is_empty() {
        return NONE;
    }
    if granted & WRITE != 0 {
        granted |= TRAVERSE
            | ENTER
            | MUTE_DEAFEN
            | MOVE
            | MAKE_CHANNEL
            | LINK_CHANNEL
            | TEXT_MESSAGE
            | MAKE_TEMP_CHANNEL
            | LISTEN;
        if channel == 0 {
            granted |= ROOT_ONLY;
        }
    }
    granted
}

/// Murmur's `Group::appliesToUser`: `current` is the channel permissions are
/// evaluated for, `acl_channel` the channel holding the ACL entry.
pub(super) fn group_applies(
    tree: &impl Tree,
    current: u32,
    acl_channel: u32,
    specification: &str,
    subject: &Subject,
) -> bool {
    let mut name = specification;
    let mut invert = false;
    let mut token = false;
    let mut hash = false;
    let mut context = current;
    loop {
        if let Some(rest) = name.strip_prefix('!') {
            invert = true;
            name = rest;
        } else if let Some(rest) = name.strip_prefix('~') {
            context = acl_channel;
            name = rest;
        } else if let Some(rest) = name.strip_prefix('#') {
            token = true;
            name = rest;
        } else if let Some(rest) = name.strip_prefix('$') {
            hash = true;
            name = rest;
        } else {
            break;
        }
    }
    if name.is_empty() {
        return false;
    }
    let matches = if token {
        let wanted = token_digest(name);
        subject.tokens.iter().fold(false, |found, held| {
            found | crate::password::equal_constant_time(held, &wanted)
        })
    } else if hash {
        subject.hash == Some(name)
    } else {
        match name {
            "none" => false,
            "all" => true,
            "auth" => subject.user_id.is_some(),
            "strong" => subject.strong,
            "in" => subject.channel == context,
            "out" => subject.channel != context,
            _ if name == "sub" || name.starts_with("sub,") => {
                match sub_matches(tree, current, context, name.get(4..).unwrap_or(""), subject) {
                    Some(matches) => matches,
                    // Murmur returns early here without applying the inversion.
                    None => return invert,
                }
            }
            _ => group_members_contain(tree, context, name, subject.user_id),
        }
    };
    matches != invert
}

/// `sub,offset,min,max`: the user is in a channel below the context channel
/// (shifted by `offset` along the evaluated channel's path), between `min` and
/// `max` levels deeper. `None` means "fails before inversion".
fn sub_matches(
    tree: &impl Tree,
    current: u32,
    context: u32,
    arguments: &str,
    subject: &Subject,
) -> Option<bool> {
    let mut parts = arguments.split(',');
    let mut next = |default: i64| {
        parts
            .next()
            .filter(|part| !part.is_empty())
            .map_or(default, leading_int)
    };
    let offset = next(0);
    let minimum = next(1);
    let maximum = next(1000);
    let home = path_from_root(tree, subject.channel);
    let path = path_from_root(tree, current);
    let index = path.iter().position(|channel| *channel == context)? as i64;
    let required = (index + offset).max(0);
    if required >= path.len() as i64 {
        return None;
    }
    let required_channel = path[required as usize];
    if !home.contains(&required_channel) {
        return None;
    }
    let depth = home.len() as i64 - 1;
    Some(depth >= required + minimum && depth <= required + maximum)
}

/// `QString::toInt` semantics for small values: optional sign and digits, else 0.
fn leading_int(text: &str) -> i64 {
    text.trim().parse::<i32>().map_or(0, i64::from)
}

/// Members of a named group as seen from `context`: walk up while groups
/// inherit, then apply add/remove lists from the root down.
fn group_members_contain(tree: &impl Tree, context: u32, name: &str, user_id: Option<u32>) -> bool {
    let Some(user_id) = user_id else {
        return false;
    };
    let mut stack = Vec::new();
    let mut current = Some(context);
    let mut steps = 0;
    while let Some(id) = current {
        let Some(node) = tree.node(id) else { break };
        steps += 1;
        if steps > 4096 {
            break;
        }
        if let Some(group) = node.groups.get(name) {
            if id != context && !group.inheritable {
                break;
            }
            stack.push(group);
            if !group.inherit {
                break;
            }
        }
        current = node.parent;
    }
    let mut matches = false;
    for group in stack.iter().rev() {
        if group.add.contains(&user_id) {
            matches = true;
        }
        if group.remove.contains(&user_id) {
            matches = false;
        }
    }
    matches
}

/// The effective member list of group `name` in `channel` (for the ACL editor).
pub fn group_members(tree: &impl Tree, channel: u32, name: &str) -> BTreeSet<u32> {
    let mut stack = Vec::new();
    let mut current = Some(channel);
    while let Some(id) = current {
        let Some(node) = tree.node(id) else { break };
        if stack.len() > 4096 {
            break;
        }
        if let Some(group) = node.groups.get(name) {
            if id != channel && !group.inheritable {
                break;
            }
            stack.push(group);
            if !group.inherit {
                break;
            }
        }
        current = node.parent;
    }
    let mut members = BTreeSet::new();
    for group in stack.iter().rev() {
        members.extend(group.add.iter().copied());
        for removed in &group.remove {
            members.remove(removed);
        }
    }
    members
}

/// The group visible under `name` from `channel`: its own, or an inheritable
/// one from an ancestor (Murmur's `Group::getGroup`).
pub fn find_group<'a>(tree: &'a impl Tree, channel: u32, name: &str) -> Option<(u32, &'a Group)> {
    let mut current = Some(channel);
    let mut steps = 0;
    while let Some(id) = current {
        let node = tree.node(id)?;
        steps += 1;
        if steps > 4096 {
            return None;
        }
        if let Some(group) = node.groups.get(name) {
            return (id == channel || group.inheritable).then_some((id, group));
        }
        current = node.parent;
    }
    None
}

/// Every group name visible in `channel` (Murmur's `Group::groupNames`).
pub fn group_names(tree: &impl Tree, channel: u32) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for id in path_from_root(tree, channel) {
        let Some(node) = tree.node(id) else { continue };
        for (name, group) in &node.groups {
            if id != channel && !group.inheritable {
                names.remove(name);
            } else {
                names.insert(name.clone());
            }
        }
    }
    names
}

/// ACL entries shown for `channel`: inherited `apply_subs` entries from
/// ancestors (while channels inherit), then the channel's own.
pub fn visible_acls(tree: &impl Tree, channel: u32) -> Vec<(u32, &AclEntry)> {
    let mut chain = Vec::new();
    let mut current = Some(channel);
    while let Some(id) = current {
        let Some(node) = tree.node(id) else { break };
        if chain.len() > 4096 {
            break;
        }
        chain.push(id);
        current = if id == channel || node.inherit_acl {
            node.parent
        } else {
            None
        };
    }
    let mut entries = Vec::new();
    for &id in chain.iter().rev() {
        let Some(node) = tree.node(id) else { continue };
        for acl in &node.acls {
            if id == channel || acl.apply_subs {
                entries.push((id, acl));
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::{perm::*, *};

    fn acl(group: &str, grant: u32, deny: u32) -> AclEntry {
        AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: None,
            group: group.into(),
            grant,
            deny,
        }
    }

    /// Root(0) ─ A(1) ─ B(2) ─ C(3); Root ─ D(4). Murmur's default root ACLs.
    fn tree() -> HashMap<u32, Node> {
        let mut admin = Group::new();
        admin.add.insert(7);
        let root = Node {
            parent: None,
            inherit_acl: true,
            acls: vec![
                acl("admin", WRITE, 0),
                acl("auth", MAKE_TEMP_CHANNEL, 0),
                acl("all", SELF_REGISTER, 0),
            ],
            groups: BTreeMap::from([("admin".into(), admin)]),
        };
        let child = |parent| Node {
            parent: Some(parent),
            inherit_acl: true,
            ..Default::default()
        };
        HashMap::from([
            (0, root),
            (1, child(0)),
            (2, child(1)),
            (3, child(2)),
            (4, child(0)),
        ])
    }

    fn subject(user_id: Option<u32>, channel: u32) -> Subject<'static> {
        Subject {
            user_id,
            channel,
            hash: None,
            tokens: &[],
            strong: false,
        }
    }

    #[test]
    fn defaults_and_murmurs_initial_root_acls() {
        let tree = tree();
        let anonymous = effective_permissions(&tree, &subject(None, 0), 0);
        assert_eq!(anonymous, DEFAULT | SELF_REGISTER);
        let registered = effective_permissions(&tree, &subject(Some(3), 0), 1);
        assert_eq!(registered, DEFAULT | MAKE_TEMP_CHANNEL);
        // Root-only permissions are not inherited by children.
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 1) & SELF_REGISTER,
            0
        );
        // Admins get Write and everything it implies; Kick/Ban only at the root.
        let admin = effective_permissions(&tree, &subject(Some(7), 0), 0);
        assert_eq!(
            admin & (WRITE | KICK | BAN | REGISTER | MOVE),
            WRITE | KICK | BAN | REGISTER | MOVE
        );
        let admin_child = effective_permissions(&tree, &subject(Some(7), 0), 2);
        assert_eq!(admin_child & (WRITE | KICK), WRITE);
        // SuperUser has everything except Speak and Whisper.
        assert_eq!(
            effective_permissions(&tree, &subject(Some(0), 0), 3),
            ALL & !(SPEAK | WHISPER)
        );
        // Unknown channels grant nothing.
        assert_eq!(effective_permissions(&tree, &subject(None, 0), 99), NONE);
    }

    #[test]
    fn deny_applies_down_the_tree_and_inheritance_can_be_cut() {
        let mut tree = tree();
        tree.get_mut(&1)
            .unwrap()
            .acls
            .push(acl("all", 0, SPEAK | TEXT_MESSAGE));
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 3) & SPEAK,
            0
        );
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 4) & SPEAK,
            SPEAK
        );
        tree.get_mut(&2).unwrap().inherit_acl = false;
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 3) & SPEAK,
            SPEAK
        );
        // apply_subs only: not in the channel itself.
        let mut tree = self::tree();
        let mut entry = acl("all", 0, ENTER);
        entry.apply_here = false;
        tree.get_mut(&1).unwrap().acls.push(entry);
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 1) & ENTER,
            ENTER
        );
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 2) & ENTER,
            0
        );
    }

    #[test]
    fn traverse_denied_above_blocks_everything_below_unless_write() {
        let mut tree = tree();
        let mut entry = acl("all", 0, TRAVERSE);
        entry.apply_subs = false;
        tree.get_mut(&1).unwrap().acls.push(entry);
        assert_eq!(effective_permissions(&tree, &subject(None, 0), 3), NONE);
        assert_eq!(effective_permissions(&tree, &subject(None, 0), 1), NONE);
        // An admin with Write keeps access.
        assert_ne!(
            effective_permissions(&tree, &subject(Some(7), 0), 3) & ENTER,
            0
        );
    }

    #[test]
    fn meta_groups_in_out_tokens_hashes_and_inversion() {
        let mut tree = tree();
        tree.get_mut(&1).unwrap().acls = vec![
            acl("all", 0, ENTER),
            acl("#Secret", ENTER, 0),
            acl("$abc", SPEAK, 0),
            acl("in", MOVE, 0),
            acl("!auth", 0, TEXT_MESSAGE),
        ];
        let tokens = vec![token_digest("SECRET")];
        let mut with_token = subject(None, 0);
        with_token.tokens = &tokens;
        assert_eq!(
            effective_permissions(&tree, &subject(None, 0), 1) & ENTER,
            0
        );
        assert_eq!(effective_permissions(&tree, &with_token, 1) & ENTER, ENTER);
        let mut hashed = subject(Some(5), 1);
        hashed.hash = Some("abc");
        let permissions = effective_permissions(&tree, &hashed, 1);
        assert_eq!(permissions & (MOVE | TEXT_MESSAGE), MOVE | TEXT_MESSAGE);
        let outside = effective_permissions(&tree, &subject(None, 0), 1);
        assert_eq!(outside & (MOVE | TEXT_MESSAGE), 0);
    }

    #[test]
    fn group_inheritance_add_remove_and_tilde() {
        let mut tree = tree();
        // Channel 2 removes user 7 from the inherited admin group.
        let mut local = Group::new();
        local.remove.insert(7);
        local.add.insert(8);
        tree.get_mut(&2)
            .unwrap()
            .groups
            .insert("admin".into(), local);
        assert_eq!(group_members(&tree, 3, "admin"), BTreeSet::from([8]));
        assert_eq!(group_members(&tree, 1, "admin"), BTreeSet::from([7]));
        assert_eq!(
            effective_permissions(&tree, &subject(Some(7), 0), 3) & WRITE,
            0
        );
        assert_eq!(
            effective_permissions(&tree, &subject(Some(8), 0), 3) & WRITE,
            WRITE
        );
        // A non-inheritable group is invisible below its channel.
        tree.get_mut(&2)
            .unwrap()
            .groups
            .get_mut("admin")
            .unwrap()
            .inheritable = false;
        assert_eq!(group_members(&tree, 3, "admin"), BTreeSet::new());
        assert!(find_group(&tree, 3, "admin").is_none());
        assert!(group_names(&tree, 3).is_empty());
        // `~in` is evaluated in the ACL's channel, `in` in the evaluated one.
        let in_one = subject(None, 1);
        assert!(group_applies(&tree, 3, 1, "~in", &in_one));
        assert!(!group_applies(&tree, 3, 1, "in", &in_one));
    }

    #[test]
    fn sub_groups_follow_murmurs_offsets() {
        let tree = tree();
        // Evaluated for C(3) with the ACL in A(1): "~sub" = A's descendants.
        let in_b = subject(None, 2);
        let in_d = subject(None, 4);
        let in_a = subject(None, 1);
        assert!(group_applies(&tree, 3, 1, "~sub", &in_b));
        assert!(!group_applies(&tree, 3, 1, "~sub", &in_d));
        assert!(
            !group_applies(&tree, 3, 1, "~sub", &in_a),
            "min depth 1 excludes A itself"
        );
        assert!(group_applies(&tree, 3, 1, "~sub,0,0", &in_a));
        assert!(!group_applies(&tree, 3, 1, "~sub,0,1,1", &subject(None, 3)));
        // Inversion does not apply when the required channel is off the path.
        assert!(group_applies(&tree, 3, 1, "!~sub,5", &in_b));
        assert!(!group_applies(&tree, 3, 1, "!~sub", &in_b));
    }

    #[test]
    fn visible_acls_list_inherited_entries_first() {
        let mut tree = tree();
        let mut only_here = acl("in", SPEAK, 0);
        only_here.apply_subs = false;
        tree.get_mut(&1).unwrap().acls.push(only_here.clone());
        tree.get_mut(&2).unwrap().acls.push(acl("out", 0, SPEAK));
        let entries = visible_acls(&tree, 2);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].0, 0);
        assert_eq!(entries[3], (2, &acl("out", 0, SPEAK)));
        // Murmur still lists the parent's entries when the channel itself does
        // not inherit (the editor shows them greyed out), but stops above a
        // parent that does not inherit.
        tree.get_mut(&2).unwrap().inherit_acl = false;
        assert_eq!(visible_acls(&tree, 2).len(), 4);
        tree.get_mut(&1).unwrap().inherit_acl = false;
        assert_eq!(visible_acls(&tree, 2).len(), 1);
    }

    #[test]
    fn hostile_group_specifications_do_not_panic() {
        let tree = tree();
        for specification in [
            "",
            "!",
            "~",
            "#",
            "$",
            "!~#$",
            "sub,",
            "sub,-99999999999,x,",
            "sub,,,,,",
            "~sub,1",
            "sub,2147483647,2147483647,2147483647",
        ] {
            let _ = group_applies(&tree, 3, 1, specification, &subject(Some(1), 3));
        }
    }
}
