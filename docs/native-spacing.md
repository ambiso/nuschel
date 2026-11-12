# Native rail and footer spacing proof

The native two-line correction reserves vertical padding on both ends of the
server rail and adds a gap to the Settings/mute/deafen control cluster. Its
runtime regression is `screenshots.py --scenes native-spacing`, implemented in
`tests/native_spacing.py`. This is a spacing proof, not a claim that the entire
chat layout is comfortable at the smallest window and highest zoom.

The exact root 1db450f3 package passed 16 cases in private GNOME: actual window
sizes 860×520 (the application's minimum), 1200×520, 1280×800 and 1600×900, each at
100/125/150/200% using real native zoom shortcuts and saved-preference checks.
The Add control's visible-frame bottom gutter was at least 13 logical pixels;
Settings/Unmute/Deafen gaps were exactly 4 logical pixels. All controls stayed
inside the compositor's visible frame, offered actions and accepted their
component hit-test centers. First/last server decoration gutters stayed inside
the real scroll viewport at both ends; scrolling did not move Add.

Actual pointer clicks opened Settings and Add at default 100% and narrow 200%.
Pointer Unmute/Mute and Deafen/Undeafen were independently acknowledged by the
owned server at 100%, ending muted. Audio was explicitly routed to the harness's
private null Pulse daemon and synthetic microphone, with the Pulse backend and
socket forced in the App environment. No host audio devices/profile/session were
used. The screenshots were visually reviewed.

The cached pre-spacing GUI baseline failed the same fixture at 100%: both footer
gaps were 0px, and its independently captured Add bottom gutter was 1px (below the
8px regression threshold). The first failing assertion was the gap assertion;
the captured bottom metric separately demonstrates the missing gutter. That
older package also differs in non-GUI protocol/browser source, so this is a
cached GUI negative control, not a claim that the whole packages differ only
by two lines. No rebuild was needed.

The fixture derives visible-frame coordinates from compositor frame/buffer
rectangles. It waits for requested valid window sizes, zoom layout and footer
resize proximity before sampling; that proximity deliberately allows 0 padding
so the negative baseline still reaches the real spacing assertions. AT-SPI's
virtual window root legitimately has unknown bounds, so it is not used as a
viewport. The retained initial trials cover a mistaken modal accessible name,
resize sampling before compositor/child geometry agreed, and that unknown root
sentinel. Those are fixture failures; production source was unchanged throughout.

Exact binaries, commands, screenshots, metrics, retained trials and cleanup are
in `~/.cache/nuschl-native-spacing/handoff.json`. Positive evidence is under
`run-4/`; the expected negative is under `negative/`.
