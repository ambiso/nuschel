# gpui-pre (vendored from crates.io, Apache-2.0)

Patched for a window zoom, Nuschel's Ctrl+= / Ctrl+- / Ctrl+0 (see the
`Nuschel patch` comments in `src/window.rs` and `src/platform.rs`), so
that custom elements can handle accessibility actions (`src/element.rs`), and
so that applications can draw emoji as their own images (`src/text_system`). The
examples and benchmarks were removed.

`Window::set_zoom(zoom, cx)` scales the whole window like a browser's page
zoom: the scale factor GPUI renders with is the platform's times `zoom`, and
the viewport, the mouse position and input events are divided by it. Layout
(pixels and rems alike), text, images, SVG icons and hit testing therefore all
change size together, and glyphs and icons are rasterised at the new size, so
they stay sharp. Geometry GPUI hands back to the platform (the client inset and
corner radii, the window menu position, input regions, `resize`, IME
positions and the input handler's text bounds) is multiplied by `zoom`.
`Window::bounds` and `window_bounds` stay in screen pixels. Accessibility
bounds already use the scale factor and need no change.

GPUI's own scaling is `set_rem_size`, which reaches only rem-based sizes;
Nuschel's interface, like most GPUI code, uses pixel sizes too. Converting
every size would touch the whole interface and keep needing care in new code.

`GlobalElementId::accesskit_node_id` is public (`src/element.rs`). A custom
element can give itself an accessibility node (`a11y_role`, `write_a11y_info`)
but had no way to know that node's id, which `Window::on_a11y_action` needs.
The desktop's timeline stands in for entries it does not lay out with such an
element: a list item for screen readers, without bounds, that scrolls the entry
into view when asked to (see `apps/desktop/src/scrolling.rs`, `Unseen`).

Font fallback keeps what was asked for (`src/text_system.rs`,
`resolve_font_uncached`). When a family cannot be resolved, GPUI tries its
fallback families; it used to try them at their default weight and style, so
bold and italic text in a missing family (Nuschel's defaults, "sans-serif" and
"monospace", on Linux before `gpui-pre-wgpu` resolved generic families) came
out as the fallback's regular face, and code came out proportional. A
fallback now has the request's weight, style, features and fallbacks, with
only the family changed (each family is looked up by itself first, so a
missing one is asked of the platform once). A monospace request (generic
names, `.ZedMono`, Lilex, and families with the word "Mono", "Code",
"Courier", "Consolas" or "Console" in their name) first tries `.ZedMono`, the
platform's generic "monospace" and common monospace families, then the
proportional stack; a proportional request no longer tries `.ZedMono` first
(it stays last, so the set of candidates is unchanged).

The fallback tests are in `font_resolution_tests` (`src/text_system.rs`).
This crate's test target does not build here: other upstream test modules
need dev-dependencies that are not in `Cargo.lock` (proptest, postage,
futures-concurrency) and Zed's assets. They run in a throwaway build that
gates every other test module out of its own copy of the source; the source
here keeps all of upstream's tests.

Emoji drawn as the application's images (`src/text_system/emoji_images.rs`,
`paint_line` in `src/text_system/line.rs`). An application registers one
`EmojiImages` source per process (`set_emoji_images`); Nuschel's is the
desktop's Twemoji (`apps/desktop/src/emoji.rs`). When GPUI paints a
`ShapedLine` or `WrappedLine` (labels, `StyledText`, text inputs), it asks the
source which byte ranges of the line's text are emoji to draw as images, and
paints each as one square image, centred over the advance its glyphs were
shaped with, instead of those glyphs. Shaping, wrapping, hit testing,
selection and underlines are unchanged; a bare `LineLayout::paint` has no
text and keeps its glyphs. The decision is made from the text, not from which
font shaped it, so text presentation (U+FE0E, and characters such as ™ that
are text by default) stays text whatever fonts are installed, and DejaVu
Sans's black-and-white smileys no longer win over colour emoji. Ranges the
source returns are checked (ascending, disjoint, on character boundaries).
Images are rasterised with GPUI's SVG renderer on first paint and kept in a
process-wide cache of at most 4096 (image, size) entries and edges up to 512
device pixels; beyond those bounds, or if an image does not render, the
glyphs are painted as before. ASCII-only lines skip the lookup.

The emoji painting has no test in this crate (see above: its test target does
not build here). The desktop tests the source's decisions
(`emoji::tests`), and `tests/emoji_smoke.py` checks the painted result in a
real window.

`WindowDecorations::Compositor` (`src/platform.rs`) asks for no decoration
mode, so the compositor picks one. xdg-decoration has three requests,
`set_mode(client_side)`, `set_mode(server_side)` and `unset_mode`, and upstream
could send only the first two. A compositor honours an explicit client-side
request even where its user asked apps to omit their decorations (niri's
`prefer-no-csd`), so "client" overrides that choice, while "server" overrides
compositors that prefer client-side ones. `gpui-pre-linux` implements it
(see its NUSCHL-PATCH.md); every other backend treats it as `Server`.

Drop this copy if upstream gains an equivalent window scale, a way for
custom elements to handle accessibility actions and a no-preference
decoration request, and keeps the requested weight and style through fallback.
