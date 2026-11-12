# Bounded native context menus

Server, channel, user, message and image context menus share one `MenuSurface` in
`apps/desktop/src/menus.rs`. Its preferred 320-pixel width and maximum height are
bounded by the actual zoom-aware window viewport. The measured popup includes a
paint gutter and the native client inset, protecting rounded window/resize edges
as well as ordinary overflow placement. The menu uses one retained scroll handle,
an always-visible scrollbar when overflowing, and full labels with native wrapping.
Long names do not widen the popup or make rows shrink over one another.

The reading region receives focus when a menu first opens. Tab and Shift+Tab move
between actions and wrap inside GPUI-base's existing focus trap. Focused controls
reuse the existing shared one-shot reveal behavior, so a clipped first/last action
scrolls into view. Arrow, Page Up/Down and Home/End scroll the reading region;
these are reading commands rather than a second action-selection implementation.
Unmodified Enter/Space activates an item once and stops propagation. Escape,
outside clicks and an item activation dismiss the menu and restore window focus;
a newly opened dialog can then focus its own input.

The popup occludes mouse interactions behind it. Wheel events must not scroll the
channel browser or timeline behind the popup, including at a menu's scroll limits.
Volume controls keep the menu open and use their existing control behavior.
Permission filtering and the actions themselves are unchanged.

The scroll/focus handles belong to the visible menu's retained element state,
keyed by its target and invocation position. Ordinary transport/settings refreshes
reuse them; they do not reset the scroll offset or refocus the reading region.
The application adds no periodic autoscroll timer or label measurement. Shared
focus reveal schedules work only on a new focus transition. The existing scrollbar
has finite activity transitions and its library-owned idle wakeup; this is not a
claim that every underlying component has no timers.

## Real private regression

```sh
TMPDIR=/owned/disk/cache/tmp NUSCHL_WL_TMP=/owned/short/cache/path \
python3 tests/context_menus.py \
  --binaries /path/to/package/bin \
  --murmur /path/to/mumble-server \
  --at-spi /path/to/at-spi2-core \
  --pulse-bin /path/to/pulseaudio/bin \
  --output /owned/disk/cache/context-menus
```

The fixture reuses the unchanged owned GNOME/null-Pulse/Murmur harness and a
private profile. A disposable root ACL exposes all real administrative entries;
no production server/configuration is touched. A 117-byte name includes a long
unbroken token and Japanese text. The menu contains 14 actual action items plus
its volume controls. Cases cover 1280×800 and 900×600 at 100%, and 900×800 at 200%.
The 200% test leaves the underlying user row visible before a real right click;
clicking an offscreen accessibility node is not a popup-placement test.

The old combined package reproduces a 640×1089 popup in a 920×820 buffer at 200%,
leaving actions unreachable. Its expected-failure evidence remains explicit.
The fixed geometry retains every action in a bounded scroll region, with first
and last actions activated by both real pointer and keyboard. Ban actions only
open a reason prompt and are cancelled; the fixture never actually moderates a
user. It also checks long-label wrapping, keyboard focus reveal/wrap, viewport
and right-edge message-popup placement, reading keys, hover, outside/Escape
dismissal, unchanged underlying channel-row positions after wheel bursts and
retained menu scroll/focus across a real periodic transport update.

The input-editing control uses a benign nickname prompt after the geometry cases.
Real pointer focus and Home/End/Left/Right edits must persist the expected nickname
in the disposable profile. This checks the actual result without assuming every
AT-SPI entry wrapper implements the Text interface. Current context-menu entries
contain no editable text field inside the menu's own reading region. The AT-SPI
cache helper returns the whole application, so the fixture explicitly filters
cached parent references when scoping a menu/dialog.

A `frame-timing` build repeats the same scenario. With a final menu action focused,
it waits for activity to settle, observes eight seconds, requires a continuous
quiet interval of at least two seconds and rejects repeated focus-reveal work.
Transport notifications still cause legitimate draws. The complete trace and
notification timestamps are retained; no zero-redraw or cross-platform performance
claim follows from this private fixture.

Final November 12 evidence is retained under
`~/.cache/nuschl-next/context-menus/`: `baseline5/` reproduces clipping with the
old combined package; `final5/` and `final5-profiled/` pass the full private
scenario. Earlier failed harness, geometry and scroll-isolation trials remain
separate. The production source is `ab9c59d2`, packaged as
`/nix/store/5qc1lbycc00xv68nx523s3w448h4m5ad-nuschl-0.1.0`; its actual scoped
notice reader passes. All 82 native desktop unit tests pass. No GPUI-kit component,
dependency, vendor or general-layout-helper changes were needed.

| Requested frame / zoom | Actual bounded popup, physical pixels |
| --- | --- |
| 1280×800 / 100% | 320×736 |
| 900×600 / 100% | 320×564 |
| 900×800 / 200% | 640×708 |

The final 8.017-second profiled idle interval contains exactly two transport
notifications, five seconds apart, and two corresponding draws 50/57 microseconds
later. There are no new focus reveals; the longest continuous quiet interval is
5.010 seconds. The full scenario records 314 draws: p95 request-layout is
0.727 ms and p95 total draw is 3.069 ms (maximum 7.907 ms). These measurements
describe this private fixture; the baseline executable was not instrumented for
a before/after performance comparison. `handoff.json` identifies exact commits,
packages, traces, screenshots and limits.

## Separate action-authority work

This is a geometry/input-routing change, not proof of stale moderation authority.
The queued follow-up must bind menu/prompt targets to server identity, connection
generation and user presence/channel authority; old server-vector/session indexes
alone are insufficient across removal/reconnect/session reuse. Another source-level
follow-up checks semantic focus identity when permission/context-action updates
insert or remove rows: current row/focus keys use vector indexes. These gates
remain in TODO and were not exercised by a fixture with settled permissions.
Windows/macOS native execution and physical assistive-reader validation also
remain separate.

## Delayed action authority and semantic focus

A menu invocation captures the saved server ID and endpoint, the native
connection generation, our observed presence, and its primary user/channel
lifetime. Channel lifetime tokens are created on insertion and preserved across
ordinary updates; removal/recreation and reconnect create new tokens. Cached
channels have no live authority. A callback resolves the saved ID again, so
removing or reordering an earlier bookmark cannot retarget a retained action.
Prompts and administrative panels retain their own origins as well.

Live callbacks run synchronously through `Driver::with_action_scope`. Its RAII
guard is private and drops before the method returns, including when the result
is an unpolled future. Scope state is thread-local, keyed to the particular
Driver, and bounded. All matching scopes are considered; a differing nested
origin fails closed. Guards have unique IDs, so an explicit out-of-order drop
cannot release another restriction. The scope captures SDK-generated requests
as well as direct desktop commands. No asynchronous task inherits ambient scope.

Native enqueue derives a minimal command envelope from the original snapshot.
Only explicit command targets and the primary subject become dependencies;
renaming a user, moving them, or deleting an unrelated candidate channel does
not revoke a useful profile/Join action. User/channel vectors, immutable
conversation IDs and crypto account IDs have a 128-entry bound. Incomplete,
oversized, nested or unsupported envelopes fail closed. Very large unlink-all
operations are not silently truncated; individual channel links remain available.

The native session actor rechecks our presence, every required lifetime, exact
conversation/account coverage, and current known permissions when it consumes
an envelope. Unknown permissions remain the server's decision. An online
registered-recipient lookup must resolve to the leased user presence; immutable
crypto account operations and known direct-conversation IDs use their explicit
semantic identity. These checks protect the client-observed model and newly
actor-admitted work. Mumble sends only numeric IDs on the wire: already-submitted
traffic and an unobserved remote reuse cannot be recalled or independently
identified by the client.

Automatic comment, description and avatar blob requests use a separate narrow
SDK metadata hook carrying original interest/lifetime tuples. Native enqueue
intersects any callback scope and the actor validates them again. `Arc<Platform>`
forwards this hook rather than falling through its generic request default.
Hash-only avatars received before our own presence wait for login; one bounded
initial pass handles them, and later updates stay incremental. The browser hook
checks connection generation and retains its existing source-ticket behavior;
it does not provide the native actor's consumption guarantee.

Desktop whisper installation and selection form one actor transaction. A shared
revisioned route becomes suspended **before** native enqueue is attempted.
A full/closed queue therefore cannot leave the old recipient sending under a new
intent. An older queued installation or Stop cannot clear a newer suspension:
only the latest revision can admit a route. Per-packet checks use one atomic
route snapshot and bounded retained target descriptors, without allocations.
Target removal/reuse, known permission revocation or reconnect suspends the route
until an explicit fresh target or normal selection. It never falls back to
another whisper or normal channel broadcast.

Local actions have separate lifecycles: cached image/message/channel-link copies
use captured values; friends, nicknames, local mute and volume use certificate
hashes or saved IDs. Bookmark editing/reordering and token persistence resolve
the original endpoint and remain available offline. Saved tokens intentionally
apply to the current connection after reconnect. General voice preference
orchestration and identity/recovery setup keep their established local/persistent
semantics; an unrelated profile presence does not authorize those operations.

Action/focus keys identify the builtin action and its concrete intent, or the
server-provided context-action identifier, rather than a vector index or label.
Toggle values and dynamic destinations get distinct keys. Duplicate server labels
remain distinct. Element keys are also namespaced by menu invocation, while the
same invocation retains scroll/focus through permission updates. Captured
callbacks must still name the current invocation and a currently available
semantic entry before they can activate it.

The separate TODO for ACL/ban response replacement, row identity and unsaved draft
overwrite remains open. These collection-editing concerns are not a claim of
this original server/connection/user/channel guard.

The focused private regression is `tests/menu_authority.py`, using the same
arguments as `tests/context_menus.py`. It uses a disposable root ACL and a small
real Murmur session-ID pool, private GNOME/null Pulse, no host devices or profiles,
and no public servers. Kick is only opened and cancelled. A stale profile query
is read-only, and stale reconnect submission uses harmless channel creation.
Source-level regressions separately cover index removal/reordering, own/session/
channel reuse, missing semantic targets, enqueue failures and rapid voice intent.
Verification results and their exact source/package paths belong in the retained
handoff; the presence of this fixture alone is not a claim that it has passed.
