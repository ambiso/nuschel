# Desktop layout contracts

Desktop chrome uses the helpers in `apps/desktop/src/layout.rs`. The contract is
about available space, not the length or spelling of a message. Server names,
channel names, archive paths and other unbroken Unicode text are never shortened
by editing the input or inserting invisible break characters.

## Width and controls

A horizontal row gives remaining width to a `flexible()` lane: zero flex basis,
zero minimum width, growth and shrink enabled. Icons and compact actions belong
in a `fixed()` lane. A zero minimum alone leaves an automatic flex basis, which
can still reserve a label's intrinsic width. Every ancestor must also be finite.

`label()` uses GPUI's native ellipsis for compact destinations and preserves the
original accessibility value and tooltip. `text()` preserves the complete text
and lets native shaping wrap it, including emergency breaks in unbroken tokens.
Neither helper counts characters, measures glyphs separately or maintains a
second text-layout cache.

Fixed controls cannot fit beside text when their total widths exceed the pane.
The conversation header therefore wraps its context and toolbar as whole lanes.
The user footer similarly wraps identity and the compact control group. Short
buttons keep their natural size; the common named button constrains long labels
to the available width and allows native wrapping. Long actions belong in the
`actions()` lane below a notice's body, not in a width reservation beside it.

## Notices and voice

The conversation's certificate, downgrade, reconnect, minimum-version, whisper,
connection and server-suggestion notices share one `Notifications` region. The
entire stack has one budget of 34% of the logical viewport height; adding another
notice does not allocate another viewport fraction. Security decisions precede
ordinary information. A notice has a fixed icon/control header and a full-width
body: a narrow pane must not squeeze a paragraph into the few pixels left beside
its close button. The complete original body remains in the accessibility tree.

Wheel scrolling and Arrow Up/Down, Page Up/Down, Home and End make all of a long
body and every notice reachable. A focused control also reveals itself. Shared
named/icon buttons and voice destination links retain a GPUI focus handle and
request autoscroll only when focus enters them. Scroll regions consume requests
from their own descendants and adjust their retained scroll handle; preceding
siblings' requests are preserved. Nested lists translate and clip the focused
child's target before passing it to the enclosing dialog. This does not continually pull the view back
while someone intentionally reads another part of the text. Modal dialogs use
the same keyboard/focus contract with their existing smooth-scroll handle.

GPUI dispatches focus listeners during its focus draw phase, where an immediate
refresh does not schedule a new frame. The helper retains one reveal intent and
schedules an ordinary next-frame refresh. A changed scroll offset schedules one
prepaint-safe animation frame; unchanged offsets schedule nothing. Controls do
not request repeated reveal while they remain focused, and inputs keep their own
IME/focus handles.

The voice footer reserves a connection-status header, signal glyph and disconnect
action independently of server/channel names. The glyph means connected, not
measured connection quality. Destination links are separate compact rows in a
bounded region and expose their full names. Transport and latency details remain
in the server's connection status and Connection panel.

## Painted decorations and animation

`decorated_slot()` reserves the declared largest paint outset plus two logical
pixels of antialiasing clearance around every scroll item. Rail icons remain
48 logical pixels; each slot owns the space for its ring and badges, including
first and last items. Scroll limits therefore include the decoration gutter,
without special margins for particular indexes. Focus reveal includes the same
outset. Unread indicators still index the actual retained scroll child slots.

A single per-server `Ease` value drives roundedness for the tile, custom icon
clipping and voice outline; the outline radius adds its four-pixel extension to
the same radius snapshot. Selection/hover retarget this state, rapid reversals
continue from its current value, and GPUI's reduced-motion setting skips the
transition. There is no separate instantaneous hover-radius style. Animation
frames stop when the transition settles; listening pulses retain their existing
bounded timer.

`--scenes rail-layout` adds a long list of offline local bookmarks and a second
independent loopback Mumble server. It checks the first/last live endpoint at
Home/End and after opposite-end scroll/focus reveal at 100/125/150/200% zoom. PNG
contours independently compare the tile and outline in the same intermediate,
hovered and resting frames. No host microphone is used.

## Scope of the audit

Migrated surfaces include the conversation header and user footer; voice status,
destinations and audio-issue link; all generic conversation banners and their
shared host; certificate/downgrade bodies and action rows; and common named/icon
buttons, settings radio/switch/slider controls, font choices/openers, modal and
nested-list focus scrolling, and tooltip bounds. Server-form action groups wrap
when editing adds another action. Composer footer labels and the update bar use
the same finite label lanes. Message bodies retain their independent selectable,
virtualized timeline layout. Menus retain their existing anchored, bounded menu
layout and keyboard navigation. Nested device/font lists retain their existing
wheel routing and retained handles while sharing keyboard/focus reveal.

GPUI's native text wrapper already supplies emergency wrapping and shaping
caches. No global minimum-size defaults, framework text cache changes or custom
per-frame shaping were introduced. A possible wrapped-to-intrinsic cache reuse
in the framework was inspected, but is not represented as a proven cause or
patched without a measured framework regression.

## Reproducing the regression checks

`tests/screenshots.py --scenes layout` uses a disposable GNOME/Wayland compositor,
private AT-SPI bus and loopback stock Mumble server. It creates a real old-namespace
key-directory sentinel so normal profile migration produces the archive notice
under a long absolute path. The server independently sends a push-to-talk
suggestion. The UI receives no injected widget state. Valid long server names
stay within the existing 128-byte validation limit.

The matrix covers normal/narrow windows, a resized sidebar, and real
100/125/150/200% page zoom. It checks the bounded common host, mandatory control
bounds, full accessible path/destination text, actual Home/End scrolling and
focus-driven reveal of actions on both notices, actual Tab, keyboard/pointer
dismissal, repeated Settings/server-form opening, and nested font-list focus
through both scroll regions. Accessibility coordinates are
compared with the compositor's actual frame/buffer insets rather than assuming
client-side decoration shadows are visible window content.

`--scenes layout-performance` uses the same fixture with the existing
`frame-timing` feature and `NUSCHL_FRAME_TIMING` log. It records idle redraws,
compositor resize sweeps and pointer drags at 100% and 125%, including actual
render/layout/draw timing distributions. Compare the same instrumented old/new
executables, helper binaries, fonts, renderer and fake-audio environment. A missing
log is a failure, not evidence of fast rendering. Host load is recorded; a shared
machine benchmark is not a claim about every device or operating system.

These are private Linux GNOME checks. They do not establish native Windows/macOS
behavior or every possible negotiated server-version/security warning. Those
warnings reuse the same migrated components; their negotiation/state tests are
separate from this visual matrix.

## Recorded Linux regression evidence

The November 12, 2026 check used instrumented desktop source `9e45af8e` before
and `74619877` after, with the same packaged sibling helpers, fonts, renderer,
private fake-audio environment and real long-notice fixture. The source matrix
passed all 12 window/sidebar/zoom combinations; the final rail check passed all
eight endpoint/zoom combinations. The old executable failed the first tile's
paint-gutter assertion (paint top 53, scroll viewport top 61 at 100%). Final desktop
unit tests passed 75/75. Native Windows/macOS GUI behavior was not measured.

Existing frame-timing instrumentation reported these milliseconds:

| Operation | Before draw p50 / p95 | After draw p50 / p95 | Before / after layout p95 |
| --- | ---: | ---: | ---: |
| Resize sweep 100% | 2.787 / 3.437 | 3.271 / 4.293 | 0.988 / 1.151 |
| Resize sweep 125% | 2.550 / 3.149 | 3.522 / 4.167 | 0.853 / 1.139 |
| Pointer resize 100% | 2.510 / 3.083 | 3.398 / 4.552 | 0.741 / 1.083 |
| Pointer resize 125% | 2.461 / 3.413 | 2.970 / 3.522 | 0.795 / 0.948 |

The complete fix costs additional drawing/layout time in this fixture; it is not
a measured speedup. The final maximum draw was 4.658 ms. Compositor lag p95 stayed
at one tick during sweeps and two ticks during pointer drags. Both versions drew
only twice during each three-second idle sample at both 100% and 125%, without a
continuous redraw loop. These are bounded samples on a shared 24-CPU host; its
one-minute load was 7.93 before and 10.11 after. Other owned builds/tests were held
during timing, but unrelated host processes were not stopped.

UI checks used the exact desktop executable with cached packaged sibling helpers
and wrapper, without changing their IPC schema. This is scoped native UI proof,
not proof of a newly rebuilt whole distribution. All fixture profiles, screenshots
and logs were on disk. A Nix daemon build-directory override was rejected as
restricted; the bounded cached desktop-leaf builds used the existing daemon
scratch setting. Client-side TMPDIR does not establish the daemon's scratch path,
and no exact daemon peak-memory measurement is claimed.
