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

Evidence is retained under `~/.cache/nuschl-next/context-menus/`, including baseline,
failed harness/geometry/scroll-isolation trials, native unit/package logs and the
final handoff. The Linux source checkpoint passes all 82 native desktop unit
tests. This work does not add GPUI-kit components, dependencies or vendor patches;
it reuses the existing GPUI-base focus trap and application layout helpers.

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
