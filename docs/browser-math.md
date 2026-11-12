# Browser formulas

Display `ContentBlock::Math` and inline `Span.math` now use native MathML.
`Message.svelte` does not scan or rewrite message text: the shared content grammar
already chooses formula spans. Code blocks, inline-code spans and links stay
literal. Ordinary text and newlines remain owned by the content document.

`MathFormula.svelte` retains the exact original source in its Source disclosure
and Copy source action. While loading, on unsupported MathML, on parser failure,
or when the bounded service is busy, source remains readable. A failed render
can be retried explicitly. Collapsed long-message previews do not parse hidden
or partial formulas; they keep the full original source for copying and render
only after expansion. No fallback inserts source as HTML.

## Dependency and projection

The dependency is exactly `temml@0.14.0`, with the npm integrity and source commit
recorded in the lockfile and `public/licenses/temml-provenance.json`. Temml has no
production package dependencies. We import its published core ESM source entry
`temml/temml.js`, excluding the default dist bundle's optional chemical/physics
extensions. No Temml stylesheet, font, auto-render pass or postprocessor is used.

The pinned `__renderToMathMLTree` API is explicitly internal/unstable upstream.
A bounded adapter projects it into a much smaller typed MathML tree. Unexpected
shapes/tags/attributes fail to original source instead of silently accepting a
new upstream DOM dialect. Upgrade tests must exercise this exact entry and tree
shape, not substitute the larger default dist bundle.

Temml's internal class/style metadata is discarded. It never becomes a DOM
attribute. The projected dialect rejects HTML/SVG, URL/event/style/class/ID
attributes, annotations and arbitrary names. Only explicit MathML tags and
bounded enumerated/numeric attributes are allowed. The receiving service checks
that dialect again. `mathElement` revalidates and builds MathML with
`createElementNS` and text nodes: no `innerHTML`, DOMParser, library DOM renderer
or HTML-string insertion.

This deliberately supports a conservative mathematical subset rather than all
Temml CSS features, references, labels, colors or extension commands. Native
browser mathematics and local system math fonts determine appearance. Unsupported
features retain their original source; no external font or URL is fetched.

The bundled notices retain the exact package MIT notice, its pinned React
utility credit, Apache-2.0 terms and source-backed historical upstream notices.
Historical supplements are identified as such, without asserting an exact fork
revision or complete legal ancestry. These files ship in the web artifact under
`licenses/`; their provenance records immutable package identity and hashes.

## Isolation, limits and reflow

Temml parses only in a lazily created module Worker. Its input is a local job
number, TeX and inline/display mode. It receives no SDK client, TLS identity,
server route, browser history or device handle. The reviewed interface uses no
network/storage/device API. Browser Workers share an origin and are not native
process sandboxes; no OS-level memory or CPU quota is claimed for JavaScript.

Each formula is at most 4,096 UTF-8 bytes. The service admits at most eight jobs
(32 KiB of source) and posts only one at a time. Every job has a two-second
absolute deadline including startup and queue time. Timeout/cancellation of the
active parser terminates its realm; replies from replaced Workers are ignored.
An unresponsive parser cannot enqueue unbounded work. Unsupported/malformed
input returns source fallback without poisoning subsequent work.

Temml runs with `trust:false`, strict parsing, throwing parse errors, at most
100 macro expansions and capped requested sizes. Each call gets a fresh
null-prototype macro dictionary: definitions never persist across formulas.
Projected replies are capped at 64 KiB, 4,096 nodes, depth 48 and 16 KiB of text.
These output caps apply after library parsing; the outside Worker deadline is
still necessary for parser work and temporary allocations.

Successful immutable trees are cached by exact TeX and display mode, at most
64 entries and 2 MiB of serialized tree/key data. The cache has no conversation identity and
confers no source authenticity. Component revisions/abort signals prevent a late
result replacing newer source. Font, theme and viewport changes reflow native
MathML; they do not rerun parsing or maintain a per-frame measurement registry.

Each formula has a bounded native scroll region, including keyboard scrolling.
Wide expressions stay inside the message on a narrow screen. The
`data-math-scroll` marker identifies these regions for any future drawer swipe
policy; the current App has no swipe implementation to change.

## Verification

`node --test web/app/src/math-render.test.js` runs seven focused cases, including
the actual pinned core API through the same Vite source resolution as production.
They cover fractions, roots, scripts, matrices, recursive macros, macro isolation,
malformed syntax/recovery, attribute rejection, collapsed previews, queue/epoch
recovery and cache accounting. Scheduler tests use a test Worker; the browser
fixture supplies the separate real-Worker proof.

`tests/web_math.py` launches a private loopback Murmur and relay and drives the
exact packaged App. It uses the existing confined browser wrapper: only owned
scratch and the read-only Nix store are exposed, with no sound/camera devices or
host audio/session sockets. It never joins voice or requests capture. Copy-source
testing intercepts the browser clipboard API, so host clipboard contents are
neither read nor written.

The fixture sends actual multiline chat through the SDK and stock server, then
checks native MathML and fraction geometry, original copy source, code grammar,
malformed/macro recovery, malicious resource canaries, narrow/desktop bounds and
appearance reflow without parser jobs. It enforces the production CSP directives
with no additional script/style/worker/font allowances. These are Chromium
fixtures at phone viewport sizes, not physical iOS/Android/Safari proof.

The exact Nix App build passed all sixteen packaged Node tests. The real-Worker
fixture passed through stock Murmur 1.5.939 under Chromium 156 at 320×640,
390×844 and 1280×800. It checked actual numerator/denominator layout, keyboard
horizontal formula scrolling, source copying, code fences/inline code, malformed
and recursive-macro recovery, same-origin CSP and zero resource-canary requests.
Theme/font-size changes added no parser jobs; capture was never requested.

Legibility screenshots use an explicitly owned fontconfig with cached full
DejaVu system fonts, including DejaVu Math TeX Gyre, and explicit sans/serif/mono/
math aliases. The earlier single-font DejaVu Sans fixture produced missing math
glyph boxes despite structurally correct MathML; it is not legibility evidence.
Run `tests/web_math.py --prepare-browser --chromium CHROMIUM --font-dir FONTS
--output SCRATCH` before the server fixture to reproduce this configuration.
No font ships in the App or is fetched over HTTP. Systems without suitable local
math glyph coverage may still show missing glyphs; source remains available.
No physical Safari/iOS/Android or assistive-technology speech proof is claimed.
