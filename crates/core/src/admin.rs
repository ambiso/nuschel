// SPDX-License-Identifier: EUPL-1.2
//! UI-independent ACL drafts. Remote snapshots are never edited in place.
//!
//! Row keys belong to one draft lifetime and are never reused, including after
//! explicit reload. Account IDs refer to the registered-user database, not to
//! online sessions. Wire correlation and connection authority belong to adapters.
use crate::model::{Acl, AclEntry, AclGroup};

pub type RowId = u64;

/// A locally correlated response. The legacy wire carries no request ID.
#[derive(Clone, Debug)]
pub struct AclReply {
    pub request: u64,
    pub result: Result<Acl, String>,
}

#[derive(Clone, Debug)]
pub struct Row<T> {
    pub id: RowId,
    pub value: T,
}

#[derive(Clone, Debug)]
pub struct AclDraft {
    baseline: Acl,
    inherit_acls: bool,
    entries: Vec<Row<AclEntry>>,
    groups: Vec<Row<AclGroup>>,
    next_row: RowId,
}

impl AclDraft {
    pub fn new(acl: Acl) -> Self {
        let mut draft = Self {
            baseline: acl.clone(),
            inherit_acls: acl.inherit_acls,
            entries: Vec::new(),
            groups: Vec::new(),
            next_row: 1,
        };
        draft.reload(acl);
        draft
    }

    /// Explicit discard/reload. Previously rendered row callbacks remain invalid.
    pub fn reload(&mut self, acl: Acl) {
        self.entries.clear();
        self.groups.clear();
        for entry in &acl.entries {
            let id = self.allocate();
            self.entries.push(Row {
                id,
                value: entry.clone(),
            });
        }
        for group in &acl.groups {
            let id = self.allocate();
            self.groups.push(Row {
                id,
                value: group.clone(),
            });
        }
        self.inherit_acls = acl.inherit_acls;
        self.baseline = acl;
    }

    fn allocate(&mut self) -> RowId {
        let id = self.next_row;
        self.next_row = self.next_row.checked_add(1).expect("ACL row ID exhausted");
        id
    }

    pub fn channel(&self) -> u32 {
        self.baseline.channel
    }
    pub fn inherit_acls(&self) -> bool {
        self.inherit_acls
    }
    pub fn set_inherit_acls(&mut self, inherit: bool) {
        self.inherit_acls = inherit;
    }
    pub fn entries(&self) -> &[Row<AclEntry>] {
        &self.entries
    }
    pub fn groups(&self) -> &[Row<AclGroup>] {
        &self.groups
    }
    pub fn baseline_matches(&self, current: &Acl) -> bool {
        &self.baseline == current
    }

    pub fn dirty(&self) -> bool {
        self.inherit_acls != self.baseline.inherit_acls
            || self.entries.len() != self.baseline.entries.len()
            || self.groups.len() != self.baseline.groups.len()
            || self
                .entries
                .iter()
                .map(|row| &row.value)
                .ne(&self.baseline.entries)
            || self
                .groups
                .iter()
                .map(|row| &row.value)
                .ne(&self.baseline.groups)
    }

    pub fn snapshot(&self) -> Acl {
        Acl {
            channel: self.channel(),
            inherit_acls: self.inherit_acls,
            entries: self.entries.iter().map(|row| row.value.clone()).collect(),
            groups: self.groups.iter().map(|row| row.value.clone()).collect(),
        }
    }

    pub fn entry_mut(&mut self, id: RowId) -> Option<&mut AclEntry> {
        self.entries
            .iter_mut()
            .find(|row| row.id == id && !row.value.inherited)
            .map(|row| &mut row.value)
    }

    pub fn add_entry(&mut self) -> Result<RowId, &'static str> {
        if self.entries.len() >= 4096 {
            return Err("The editor supports at most 4096 rules.");
        }
        let id = self.allocate();
        self.entries.push(Row {
            id,
            value: AclEntry {
                apply_here: true,
                apply_subs: true,
                inherited: false,
                user_id: None,
                group: Some("all".into()),
                grant: 0,
                deny: 0,
            },
        });
        Ok(id)
    }

    pub fn remove_entry(&mut self, id: RowId) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .position(|row| row.id == id && !row.value.inherited)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }

    /// Move only local rules. Inherited rules retain their position and authority.
    pub fn move_entry(&mut self, id: RowId, down: bool) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|row| row.id == id && !row.value.inherited)
        else {
            return false;
        };
        let Some(other) = (if down {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        }) else {
            return false;
        };
        if self
            .entries
            .get(other)
            .is_none_or(|row| row.value.inherited)
        {
            return false;
        }
        self.entries.swap(index, other);
        true
    }

    pub fn group_mut(&mut self, id: RowId) -> Option<&mut AclGroup> {
        self.groups
            .iter_mut()
            .find(|row| row.id == id)
            .map(|row| &mut row.value)
    }

    pub fn add_group(&mut self, name: &str) -> Result<RowId, &'static str> {
        if self.groups.len() >= 1024 {
            return Err("The editor supports at most 1024 groups.");
        }
        let name = name.trim().to_lowercase();
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            return Err("Use a group name between 1 and 256 bytes without control characters.");
        }
        if self
            .groups
            .iter()
            .any(|row| row.value.name.eq_ignore_ascii_case(&name))
        {
            return Err("That group already exists. Select it to edit its local overrides.");
        }
        let id = self.allocate();
        self.groups.push(Row {
            id,
            value: AclGroup {
                name,
                inherited: false,
                inherit: true,
                inheritable: true,
                add: Vec::new(),
                remove: Vec::new(),
                inherited_members: Vec::new(),
            },
        });
        Ok(id)
    }

    /// An inherited group cannot be deleted at its parent. Removing its local
    /// definition restores the inherited defaults, exactly as stock Mumble does.
    pub fn remove_group(&mut self, id: RowId) -> bool {
        let Some(index) = self.groups.iter().position(|row| row.id == id) else {
            return false;
        };
        if self.groups[index].value.inherited {
            let group = &mut self.groups[index].value;
            group.inherit = true;
            group.inheritable = true;
            group.add.clear();
            group.remove.clear();
        } else {
            self.groups.remove(index);
        }
        true
    }
}

/// Query provenance (`inherited`) does not mean there is no local definition.
/// Preserve inherited groups whose local flags or membership override a parent.
pub fn group_has_local_definition(group: &AclGroup) -> bool {
    !group.inherited
        || !group.inherit
        || !group.inheritable
        || !group.add.is_empty()
        || !group.remove.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl() -> Acl {
        Acl {
            channel: 7,
            inherit_acls: true,
            entries: vec![
                AclEntry {
                    apply_here: true,
                    apply_subs: true,
                    inherited: true,
                    user_id: None,
                    group: Some("admin".into()),
                    grant: 1,
                    deny: 0,
                },
                AclEntry {
                    apply_here: false,
                    apply_subs: true,
                    inherited: false,
                    user_id: Some(9876),
                    group: None,
                    grant: 0x8000_0000,
                    deny: 0x4000_0000,
                },
            ],
            groups: vec![AclGroup {
                name: "admin".into(),
                inherited: true,
                inherit: true,
                inheritable: true,
                add: vec![],
                remove: vec![],
                inherited_members: vec![42],
            }],
        }
    }

    #[test]
    fn changes_are_private_and_preserve_unedited_bits_and_fields() {
        let remote = acl();
        let mut draft = AclDraft::new(remote.clone());
        let id = draft.entries()[1].id;
        draft.entry_mut(id).unwrap().grant |= 8;
        assert!(draft.dirty());
        assert!(draft.baseline_matches(&remote));
        assert_eq!(remote, acl());
        let updated = draft.snapshot();
        assert_eq!(updated.entries[0], remote.entries[0]);
        assert_eq!(updated.groups, remote.groups);
        assert_eq!(updated.entries[1].grant, 0x8000_0008);
        assert_eq!(updated.entries[1].deny, 0x4000_0000);
        assert!(!updated.entries[1].apply_here);
        assert_eq!(updated.entries[1].user_id, Some(9876));
    }

    #[test]
    fn row_actions_cannot_retarget_after_remove_reorder_or_reload() {
        let mut draft = AclDraft::new(acl());
        let inherited = draft.entries()[0].id;
        assert!(draft.entry_mut(inherited).is_none());
        assert!(!draft.remove_entry(inherited));
        let old = draft.entries()[1].id;
        let new = draft.add_entry().unwrap();
        assert!(draft.move_entry(new, false));
        assert_eq!(draft.entries()[1].id, new);
        assert_eq!(draft.entry_mut(old).unwrap().user_id, Some(9876));
        assert!(draft.remove_entry(old));
        assert!(draft.entry_mut(old).is_none());
        draft.reload(acl());
        assert!(draft.entry_mut(new).is_none());
        assert!(draft.entry_mut(old).is_none());
        assert!(!draft.dirty());
    }

    #[test]
    fn inherited_group_overrides_are_editable_and_reset_locally() {
        let mut draft = AclDraft::new(acl());
        let id = draft.groups()[0].id;
        assert!(!group_has_local_definition(&draft.groups()[0].value));
        let group = draft.group_mut(id).unwrap();
        group.add.push(9000); // Registered account ID, independent of presence.
        group.remove.push(42);
        group.inheritable = false;
        assert!(group_has_local_definition(&draft.groups()[0].value));
        assert!(draft.remove_group(id));
        let group = &draft.groups()[0].value;
        assert_eq!(group.inherited_members, vec![42]);
        assert!(!group_has_local_definition(group));
        assert!(!draft.dirty());
    }

    #[test]
    fn named_group_creation_and_baseline_conflicts_are_explicit() {
        let mut draft = AclDraft::new(acl());
        assert!(draft.add_group("ADMIN").is_err());
        assert!(draft.add_group("").is_err());
        let id = draft.add_group("  Music  ").unwrap();
        assert_eq!(draft.group_mut(id).unwrap().name, "music");
        assert!(draft.remove_group(id));
        assert!(!draft.dirty());
        let mut changed = acl();
        changed.groups[0].inherited_members.push(55);
        assert!(!draft.baseline_matches(&changed));
        assert!(draft.baseline_matches(&acl()));
    }
}
