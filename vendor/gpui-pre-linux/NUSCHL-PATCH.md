# Typed image clipboard writes

Based on the published `gpui-pre-linux` 0.3.11 snapshot (Zed f816e1f), Apache-2.0.
The original source and license are retained; registry metadata and the library's
unused Cargo.lock are omitted.

The upstream clipboard accepted `ClipboardEntry::Image` internally but advertised
and served only text to other Linux applications. Wayland now advertises the
actual image MIME types and serves only the bytes matching each requested type.
Image-only selections do not advertise empty text. Text and primary-selection
behavior remain separate. X11 now calls its existing encoded-image setter, and
that setter uses the selected image format's atom instead of always claiming PNG.
No image decoding or new unsafe operations are added to the platform backend.

Nuschl's Copy image action supplies a PNG encoded from already validated displayed
pixels. A private GNOME fixture reads `image/png` independently and compares its
decoded pixels with the generated test image (`tests/screenshots.py`, viewer scene).
Pure MIME/payload regressions live beside the Wayland implementation. Remove this
patch once the upstream backend provides equivalent typed clipboard support.
