# Browser formulas

Display `ContentBlock::Math` and inline `Span.math` now use native MathML.
`Message.svelte` does not scan or rewrite message text: the shared content grammar
already chooses formula spans. Code blocks, inline-code spans and links stay
literal. Ordinary text and newlines remain owned by the content document.

`MathFormula.svelte` retains the exact original source in its Source disclosure
and Copy source action. While loading, on unsupported MathML, on parser failure,
or when the bounded service is busy, source remains readable. A failed render
can be retried explicitly while its worker remains healthy. Permanent worker
failure offers **Reload client** instead of an ineffective retry. Collapsed long-message previews do not parse hidden
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

Each formula is at most 4,096 UTF-8 bytes. All `MathRenderService` facades in
one page/module realm share **one** lazy worker owner and at most eight jobs /
32 KiB of source, with only one posted at a time. Content processing uses a
separate owner and limits. Both use the private `worker-owner.js` scheduler;
formula and content messages, validators, caches and workers remain distinct.
There is no public reset/replacement operation. Creating or disposing facades
cannot manufacture another owner or cancel another facade's jobs. The first
admitted request fixes the factory and startup options; unused constructors do
not claim that lease. Diagnostics expose only detached frozen numeric snapshots.

A formula job's two-second execution deadline starts when it is dispatched to
the worker and retires the formula owner if missed; time waiting for startup or
in the queue has a separate reject-only two-second deadline from admission.
The loader has a **separate original fifteen-second ceiling**;
new consumers and duplicate readiness cannot extend it. A queued first formula
may time out to source while the cold module continues loading. Once that same
worker becomes ready, an explicit Render formula retry can succeed. Absolute
deadlines are checked on readiness and replies as well as by timers, so delayed
main-thread timer delivery cannot admit late work.

Queued cancellation/expiry releases unposted work immediately. Active
cancellation settles only that consumer, retaining its slot, source-byte charge
and original deadline until the **exact actual result/error** arrives. Its
result is discarded, never reattached or cached. Responsive canceled work,
ordinary syntax errors, recursive-macro rejection and facade disposal reuse the
same healthy worker. Disposal does not clear the shared cache.

An active no-ACK deadline, startup failure, trap, post failure or malformed/stale
reply permanently retires the page's owner. All consumers settle even when
`terminate()` throws; the consumed owner and active byte charge remain reserved.
Late replies cannot deliver a derivative, populate the cache or reopen capacity,
and twenty new facades cannot bypass this rule. Worker termination has no
synchronous physical-exit acknowledgment; this proves bounded logical ownership
and no automatic replacement, not immediate thread death or OS resource quotas.

Mounted formula views explicitly unsubscribe from retirement notifications on
destruction. Already rendered formulas and immutable cached results remain
usable after another formula retires the worker. Uncached pending/failed views
preserve source and offer Reload client; ordinary formula errors retain Render
formula. Reload is a user action, never automatic. The composer, SDK, bookmarks
and history are not cleared by a math failure. As with SDK recovery, drafts are
page memory: copy an unsent draft before explicitly reloading. This change adds
neither draft persistence nor the pending content/attachment broker.

Temml runs with `trust:false`, strict parsing, throwing parse errors, at most
100 macro expansions and capped requested sizes. Each call gets a fresh
null-prototype macro dictionary: definitions never persist across formulas.
Projected replies are capped at 64 KiB, 4,096 nodes, depth 48 and 16 KiB of text.
These output caps apply after library parsing; the outside Worker deadline is
still necessary for parser work and temporary allocations.

Successful immutable trees share one page/module cache keyed by exact TeX and
display mode, at most 64 entries and 2 MiB of serialized tree/key data. The cache has no conversation identity and
confers no source authenticity. Component revisions/abort signals prevent a late
result replacing newer source. Font, theme and viewport changes reflow native
MathML; they do not rerun parsing or maintain a per-frame measurement registry.

Each formula has a bounded native scroll region, including keyboard scrolling.
Wide expressions stay inside the message on a narrow screen. The
`data-math-scroll` marker identifies these regions for any future drawer swipe
policy; the current App has no swipe implementation to change.

## Verification

The packaged Nix check runs **32 Node tests**, including four actual pinned
Temml core/grammar cases through production Vite resolution and fourteen math
lifecycle cases in `math-render-lifecycle.test.js`. Lifecycle cases isolate test
module realms, not a public production reset. They cover shared budgets/cache,
healthy cancellation/ACK reuse, startup cancellation, original deadlines and
late-ready/result races, first-job expiry then same-loader retry, bounded new
facades, failed factory/post/termination, traps/malformed/stale replies,
subscriber cleanup, immutable cache eviction, detached diagnostics and finite
option bounds. The shared content adapter's original seventeen independent
checks also pass: `node --test tests/web_content.unit.mjs`.

`tests/web_math_retirement.browser.mjs SOURCE SITE WRAPPER PLAYWRIGHT OUTPUT`
uses the actual packaged Temml worker with a source scheduling adapter. Its
fixture-only HTTP transformation appends an ordinary worker-message busy task;
production worker code/Temml are not replaced or evaluated through an inspector.
It proves cold module loading beyond the first two-second job, same-owner
healthy cancellation, normal and throwing termination after an actual 3.5-second
stall, twenty-facade rejection and an actual late old Temml result being ignored.
It retains valid cached trees/fraction DOM and exercises macro/resource canaries
under production CSP. Content's actual-WASM code/image/canary and normal/throwing
retirement cases are rerun after the scheduler extraction, not inferred from
unit tests alone.

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

The earlier math baseline passed sixteen packaged Node tests; the lifecycle
App build now passes all thirty-two. The real-Worker
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

The lifecycle App proof uses `NUSCHL_MATH_RETIREMENT_PROOF=1` with
`tests/web_math.py`. It delays only the phone's first Temml module response by
2.5 seconds, verifies readable two-second source fallback, opens the native
Source disclosure, clicks the actual visible Render formula button and requires
a real new worker post before checking recovered MathML on that same owner.
A later busy task proves permanent source/Reload recovery at 320 pixels, retained
valid fraction DOM and unsent draft, and no new posts/worker for an uncached
subsequent formula. It checks original source, CSP/canaries and all ordinary App
geometry/reflow cases too. No microphone call or host clipboard access occurs.

Source checkpoints are `87fae40c` (private scheduler/content extraction) and
`585bb33f` (math owner/UI). The exact assembled App is
`/nix/store/rjaarc0s8srprv7a7vq7zsxmf3z6ln9f-nuschl-web-app-with-notices`.
Source-bound commands, hashes, screenshots and limitations are in
`~/.cache/nuschl-math-retirement/handoff.json`; actual proofs are
`browser/proof.json`, `content-browser/proof.json` and
`app-cold-final/proof.json` there. The three earlier stricter App failures remain
in `app-final`, `app-retry-debug` and `app-tags-debug`: the fixture counted a
button by accessible role while its native details disclosure was closed, so no
retry click happened. Tag/page diagnostics established that distinction; the
final fixture opens Source first and asserts the real worker post. No production
change was made to satisfy those failed fixture preconditions.
