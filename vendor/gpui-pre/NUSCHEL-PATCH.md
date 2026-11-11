# gpui-pre (vendored from crates.io, Apache-2.0)

Patched for a window zoom, Nuschel's Ctrl+= / Ctrl+- / Ctrl+0 (see the
`Nuschel patch` comments in `src/window.rs` and `src/platform.rs`). The
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

Drop this copy if upstream gains an equivalent window scale.
