# Saved server order

The desktop rail and browser rail support persistent drag and drop. Moving a
server changes its presentation position; it does not reconnect it, switch the
conversation, join voice, request microphone permission or replace an audio route.

On desktop, drag an icon to its new position. Holding near either edge scrolls
the rail. Escape or dropping outside the rail cancels. A focused server also has
Ctrl+Shift+Up/Down and context-menu Move earlier/later alternatives.

In the browser, desktop pointers can drag icons directly. On touch screens,
Reorder servers exposes dedicated handles, leaving ordinary icon gestures free
to scroll the drawer. Handles support arrow keys; focused icons also support
Ctrl+Shift+Up/Down. Server settings expose Move earlier/later buttons. Escape,
pointer cancellation and viewport changes cancel a preview. Touch capture stays
on the stationary rail, and its ghost belongs to the mobile modal layer.

Both adapters animate position changes and keep the source's decoration gutter.
Reduced motion removes position animation. Animation frames and edge scrolling
stop once there is no change. No bookmark write occurs while dragging.

## SDK and persistence contract

`Client.servers` retains its runtime order. Async connection tasks and SDK commands
continue to address its original indexes. A separate catalog stores stable server
IDs and a cached ordered-index sequence. `ordered_servers()` returns each actual
runtime index together with its server; consumers must not substitute the loop's
display position. The cache is rebuilt only when catalog contents/order change.

`set_server_order()` accepts exactly one copy of every current server ID. Missing,
unknown or duplicate IDs are rejected without changing anything. It performs no
platform IO. Adding/removing a server updates the catalog; replacing a bookmark
preserves its position. `ordered_bookmarks()` serializes canonical order, and
`order_bookmarks()` orders pending add/remove/edit snapshots without dropping
new metadata. Native, TUI and browser bookmark writers use these barriers, so an
unrelated save cannot restore old runtime order.

A drag preview is local state with a frozen baseline. Concurrent catalog changes
invalidate it. A successful Drop issues one order commit. Native persistence uses
the existing atomic bookmark writer and rolls back order on failure. The browser
worker serializes order commits and acknowledges only after the IndexedDB
transaction succeeds. Generic bookmark refresh cannot queue a stale snapshot
inside that transaction. Failure restores prior positions against the current
catalog, retaining concurrently added servers and edited bookmark data. The UI
reports the failure instead of pretending it saved.

Browser snapshots include `runtime_index`, and the worker advertises
`server_order` support. Every App server command, including voice controls after
a reorder, uses this explicit runtime index. A capable worker with an invalid
index permutation fails closed; old workers keep their original unsorted
position-based behavior. Reordering never changes certificate trust, connection
or voice generations, history tickets, selected conversations or draft ownership.

## Verification

The SDK suite passed 87 tests with one pre-existing ignored test. Ordering tests
cover exact permutations, invalid/no-op requests, live-object/index stability,
voice/navigation/history/generation/pin invariants, add/remove/replace,
serialization/reload, pending writers and late async input. Native tests passed
79 cases, including reversible previews, interruption continuity, reduced motion
and settled animation. Five pure browser ordering tests and three actual SDK
worker transaction tests cover runtime mapping, commit acknowledgment, storage
failure, concurrent additions and serialized commits. The three existing worker
voice tests also passed.

`tests/screenshots.py --scenes server-order` runs actual private GNOME sessions
with twenty bookmarks and two independent loopback Murmur servers. At 100% and
200% zoom it checks drag preview without writes, Drop, Escape/outside cancellation,
keyboard/context alternatives, stationary edge scrolling, unchanged connected
sessions, a draft independently received on the original text server, and a
real process restart with saved order. The final settled check produced two draw
frames in three seconds; this is evidence against an idle animation loop, not a
claim about all hardware performance.

`tests/web_server_order.mjs` uses the packaged App, actual WASM SDK, worker and
IndexedDB under the production CSP. Desktop 1440×960 and touch 320×640/390×844
cases passed drag, cancellation, explicit runtime-index routing, accessible moves,
reload, ghost alignment and first/last decoration bounds. The six-case existing
responsive matrix passed as well. These cases connect to no servers and request
no microphone.

The private-server `tests/web_voice_transport_real.py` proof additionally passed
actual packaged App/Opus/worklet interchange with synthetic 997 Hz audio. During
a live call it added an offline bookmark and moved it ahead of the active server:
connection/voice generations, microphone count, conversation and draft stayed
unchanged. A later mute still targeted the original runtime index. Twenty audio
packets produced zero timeline snapshots; decoded output had finite nonzero RMS,
and deafen output was zero. All network targets and relay credentials were local.

The frozen pre-order integrated `12d6695c` whole Linux package also built and
passed its controls and eight rail-decoration cases. New ordering native UI proof
used a separately built desktop leaf with that package's unchanged helpers;
browser proof used its exact packaged WASM/site. A new combined whole-distribution
build, physical touch devices, Safari, Windows/macOS drag behavior, real headsets
and complete desktop feature parity remain separate gates.

Local proof records are under `~/.cache/nuschl-server-order/`: `ui-final-idle/`,
`ui-final-200/`, `browser-owned-pointer/`, `responsive-final/`,
`live-voice-order/` and `package-build/`. No host microphone, clipboard, public
server or public site is used by these fixtures.
