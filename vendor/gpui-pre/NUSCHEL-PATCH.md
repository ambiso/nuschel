# gpui-pre (vendored from crates.io, Apache-2.0)

Patched for a window zoom, Nuschel's Ctrl+= / Ctrl+- / Ctrl+0 (see the
`Nuschel patch` comments in `src/window.rs` and `src/platform.rs`), and so
that custom elements can handle accessibility actions (`src/element.rs`). The
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
"monospace", on Linux before the generic families below) came out as the
fallback's regular face, and code came out proportional. A fallback now has
the request's weight, style, features and fallbacks, with only the family
changed. A monospace request (generic names, `.ZedMono`, Lilex, and families
named "… Mono", "… Code", "Courier …" or "Consol…") first tries `.ZedMono`,
the platform's generic "monospace" and common monospace families, then the
proportional stack; a proportional request no longer tries `.ZedMono` first
(it stays last, so the set of candidates is unchanged).

Drop this copy if upstream gains an equivalent window scale and a way for
custom elements to handle accessibility actions, and keeps the requested
weight and style through fallback.
