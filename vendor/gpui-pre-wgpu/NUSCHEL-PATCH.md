# gpui-pre-wgpu (vendored from crates.io, Apache-2.0)

Patched so colour emoji render on Linux; see the `Nuschel patch` comment in
`src/cosmic_text_system.rs`. GPUI only loads font families that have a glyph
for "m", including fallback families. Colour emoji fonts (Noto Color Emoji)
have none, so text fell back to the UI font's black-and-white emoji (DejaVu
Sans has the classic smileys). Known emoji fonts are now exempt. The
benchmarks were removed.

Generic families. The CSS generic names "sans-serif" (also "sans",
"system-ui"), "serif" and "monospace" name no font face, so they never
resolved and GPUI's fallback drew them (see `gpui-pre`'s note). `load_family`
now maps them to the first installed family of a list: for sans-serif GPUI's
own fallback order (IBM Plex Sans, Helvetica, Segoe UI, Ubuntu, Adwaita Sans,
Cantarell, Noto Sans, DejaVu Sans, Arial, Liberation Sans), so the family
that used to be reached through fallback is kept; for monospace Lilex, IBM
Plex Mono, Menlo, Consolas, Ubuntu Mono, Adwaita Mono, Source Code Pro, Noto
Sans Mono, DejaVu Sans Mono, Liberation Mono, Courier New, then any family
with a face marked monospaced; for serif IBM Plex Serif, Noto Serif, DejaVu
Serif, Liberation Serif, Times New Roman, Georgia. This is not fontconfig's
own choice: cosmic-text's `FontSystem::new` overwrites the aliases fontdb
reads from fontconfig, and resolving them properly needs fontconfig itself.

Variable weights. A variable font has one face for every weight (Adwaita
Sans, GNOME's interface font, is one file upright and one italic); GPUI used
the face as it is, at its default weight, so bold text in it was regular.
`font_id` now gives a variable face whose `wght` axis reaches the requested
weight its own `FontId`: an instance (`LoadedFont::variable_weight`) that is
shaped at that weight (cosmic-text's font for the face and weight, and the
shaping attributes' weight) and drawn, measured and advanced with the axis
at that weight (`weight_coords` for swash's scaler and metrics). Faces
cosmic-text falls back to for missing glyphs are instanced at the weight the
glyph was shaped with. Static faces and regular text are unchanged.

Tests. `nuschel_tests` in `src/cosmic_text_system.rs` covers the patches; its
tests with real fonts read DejaVu and Adwaita font files from
`NUSCHEL_TEST_FONT_DIRS` and are skipped without it. Upstream's tests are kept
as they are, but this crate's test target does not build here (it did not
before these patches either): `cosmic_text_system::tests` include Zed's
bundled fonts (`assets/fonts`) and the renderer's tests need dev-dependencies,
none of which are vendored. Running `nuschel_tests` therefore takes a
throwaway build that gates those upstream modules out of its own copy of the
source; the source here is not changed for it.

Drop this copy once upstream allows emoji fonts, resolves generic families
and draws variable fonts at the requested weight.
