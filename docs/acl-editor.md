# ACL and group editor

Native editor work is in progress. This document describes the shared draft/API
boundary; it does not claim desktop, web, or TUI feature parity yet.

`nuschl_core::admin::AclDraft` owns an immutable server baseline and private local
rows. Callbacks use row IDs, never vector positions. Removal and explicit reload
retire old IDs; inherited rules remain read-only. Group provenance is separate
from local overrides: an inherited group can have local members, exclusions, or
inherit/inheritable flags. Removing its local definition resets those overrides;
it cannot remove the parent's group. Account IDs identify registered users,
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

The native panel must retain its existing `ActionOrigin` connection-generation,
own-presence, and channel-instance guards. It should use a virtualized list plus
one selected rule/group editor, bounded member lists, stable semantic focus, and
the shared modal/scroll layout. It must not instantiate all 4096 rules multiplied
by permission controls, or scan every group member during each frame.

Focused tests cover core row lifetime, inherited-group overrides, immutable
baseline/conflict behavior, dirty/late replies, wrong-channel/lossy replies,
explicit reload, frozen saves, actor query reservations after reopening and
channel ID reuse, incomplete snapshots, unresolved-query bounds, inherited-group
wire serialization, unknown permission bits, and SDK label budgets under large
groups, duplicates, and command-queue pressure. Actual native editor/server UI
proof remains outstanding.
