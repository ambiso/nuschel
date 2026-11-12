# ACL and group editor

The native client provides a local ACL/group draft editor. Right-click a channel
and choose **Edit permissions…**. Web and TUI editor parity remain queued; the
shared draft/API boundary below is intended for those frontends too.

Select a rule to edit its group expression or registered-user target, scope and
grant/deny bits. Apply or explicitly discard a typed target before saving,
creating another rule, or changing selection. Rules can be created, removed and
reordered; inherited rules are inspectable but read-only. Channel inheritance
and group inherit/inheritable flags are separate controls. Select a group to add
or remove local members, exclude inherited members, or reset a local override.
Names resolve freshly to registered account IDs; `#id` explicitly selects a
numeric account. Missing labels never discard membership.

Each open editor has its own lifetime token, in addition to stable row IDs and
original server/connection/channel authority. Dirty drafts survive disconnect as
read-only inspectors; obsolete callbacks cannot edit a replacement panel. Inputs
are read-only throughout a pending load/save/verification, and only become
editable again when bound to a current local row. Explicit reload discards local
changes after a successful response. The editor does not mutate remote cached
ACL vectors.

Rules, groups and memberships use fixed-height virtual lists with one selected
item's detail editor. Only visible rows instantiate controls. Compact list labels
ellipsize with full accessibility names and hover text; detail fields retain the
full source. Home/End, arrows and Page Up/Down navigate retained list focus, and
the shared scroll contract reveals that focus once without idle timers or
per-frame model scans. Text input keeps its native editing keys.

`nuschl_core::admin::AclDraft` owns an immutable server baseline and private local
rows. Callbacks use row IDs, never vector positions. Removal and explicit reload
retire old IDs; inherited rules remain read-only. Group provenance is separate
from local overrides: an inherited group can have local members, exclusions, or
inherit/inheritable flags. Removing its local definition resets those overrides;
it cannot remove the parent's group. The legacy query cannot distinguish a
pristine inherited-only group from an explicit empty local definition with the
same default flags. Like stock Mumble, saving omits that ambiguous definition;
its independent lifetime after an ancestor removes the group cannot be preserved
from this wire representation. Account IDs identify registered users,
including offline users, and must never be populated from presence/session IDs.
Unedited model fields and unknown permission bits survive edits.

`AclEditorState::new(channel)` manages load, explicit reload, pre-save conflict
checks, and post-save verification. A frontend supplies distinct request tickets
and preserves its original connection/channel authority. It must forward only
matching active-editor responses. Late or unsolicited replies cannot replace a
draft. Reload discards changes only after an explicit request succeeds. Save
checks a fresh snapshot against the baseline before returning a write effect;
the submitted draft remains frozen through verification. A different readback
retains the draft and reports the difference. Failed/rejected/oversized replies
also retain it. A UI timeout stops waiting, but does not free the actor's pending
wire reservation.

The native session's `QueryAclDraft` has at most one unresolved query per channel
ID and 128 across a connection. Each reservation retains its channel instance
and local ticket until the actual response or connection teardown. Closing an
editor, timing out, removing a channel, or reusing its ID must not purge that
reservation and associate the old response with a new query. A retry reports the
unresolved request instead. Legacy `QueryAcl` keeps its existing event shape;
untagged/cached ACL events cannot initialize the new editor. `QueryUserIds`
resolves names through Mumble's `QueryUsers`, independently of online presence.

The draft path refuses snapshots above 4096 rules, 1024 groups, 256-byte group
names/expressions, or 8192 total member references in one group. Duplicate group
names are also refused. These are safe editing limits, not an instruction to
truncate before saving. Different servers can impose lower write limits; the
frontend must show failures/readback differences without dropping the draft.

Name labels have a separate budget from ACL data. Both legacy and tagged SDK
responses inspect at most 4096 references, issue at most one deduplicated 128-ID
lookup per response, and spend at most 1024 ID attempts per connection. Failed
enqueues spend attempt budget, preventing repeated snapshots from hammering a
full command queue. Successful outstanding lookups are deduplicated. The label
cache retains at most 4096 account IDs. Missing names display as `#id`; they must
not make an ACL partial or cause membership to be discarded. Explicit user name
lookup remains available independently of automatic labeling.

The legacy wire has no query IDs, ACL revision, acknowledgement, or atomic
compare-and-swap. Local tickets correlate the session's serialized normal query
lane; they cannot authenticate the intent of an unsolicited same-channel reply
sent by a server while a query is outstanding. A concurrent operator can change
permissions between the preflight read and write. Post-write readback verifies
the observed local settings; it does not make that sequence atomic. Protobuf
unknown fields are already discarded by the current generated decoder; this
work does not claim opaque unknown-field roundtripping.

Fresh typed-name lookup uses a separate bounded `LookupAccount` transaction,
matched to the original editor, row and input. It first sends a Ping barrier,
then a single name query, holding ordinary name queries until completion. The
barrier preserves all current transport counters and is handled outside ordinary
RTT sampling. This relies on the honest server's ordered TLS handling; Mumble
has no authenticated query intent or immutable name-to-account transaction.
Timeouts retain the unresolved wire reservation until the actual reply or
reconnect. A changed input cannot apply an earlier result.

Native ACL reads/writes are paced against shared conservative control credit so
Murmur's default 1/s, burst-5 limit does not silently drop a load/check/write/verify
sequence. At most 16 semantic commands and 16 MiB wait; original authority is
rechecked at actual dispatch. Serialized frames are never held after a one-time
authorization. Urgent mute/deafen/privacy controls bypass the admin queue and
still spend shared credit. Queue refusals and timeouts terminate the matching UI
request while preserving its draft; writes are not blindly retried.

ACL editing follows the server's child-Write **or root-Write** administrative
rule. Current known denials are checked locally; an unknown alternative remains
subject to authoritative server rejection. Opening a menu requests missing
current permission information with bounded lifetime-aware deduplication. This
exception does not grant unrelated channel-edit/remove actions.

Focused tests cover core row lifetime, inherited-group overrides, immutable
baseline/conflict behavior, dirty/late replies, wrong-channel/lossy replies,
explicit reload, frozen saves, actor query reservations after reopening and
channel ID reuse, incomplete snapshots, unresolved-query bounds, inherited-group
wire serialization, unknown permission bits, and SDK label budgets under large
groups, duplicates, and command-queue pressure. Actual native editor/server UI
proof is exercised by [acl_editor.py](../tests/acl_editor.py) against owned stock
Murmur and nuschld at their default 1/s, burst-5 rate with no reconnect settling
or raised-limit override. It checks native offline-member editing, inherited
local overrides, ordered user/group rules, subchannel permission denial with an
unrelated control, dirty conflict retention, root-admin recovery of a child that
denies Write, stale callbacks, reconnect and restart persistence.

The same fixture's `--geometry` mode loads 512 rules and 512 groups, checking
pointer/keyboard endpoints, bounded instantiated rows, focus reveal and text
editing keys at 1280×800, 900×600 and 900×800 at 200% zoom. `--pending-input`
attempts a real edit during a naturally paced save and verifies the original
field value is retained, then editable after confirmation.
[acl_e2ee_bootstrap.py](../tests/acl_e2ee_bootstrap.py) separately starts nuschld
in E2EE-only mode and uses the native SuperUser password form and group editor
to promote an offline account, without changing security policy or using a raw
legacy operator. These are loopback protocol/admin proofs, not MLS chat/video
coverage. Both scripts use private GNOME, null audio and temporary profiles.
