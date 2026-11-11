# gpui-pre-wgpu (vendored from crates.io, Apache-2.0)

Patched so colour emoji render on Linux; see the `Nuschel patch` comment in
`src/cosmic_text_system.rs`. GPUI only loads font families that have a glyph
for "m", including fallback families. Colour emoji fonts (Noto Color Emoji)
have none, so text fell back to the UI font's black-and-white emoji (DejaVu
Sans has the classic smileys). Known emoji fonts are now exempt. The
benchmarks were removed. Drop this copy once upstream allows emoji fonts.
