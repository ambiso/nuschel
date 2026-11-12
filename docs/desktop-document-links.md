# Native document links

Welcome cards, channel descriptions and profile comments use the content
worker's typed document and safe URL spans. `BlockText` keeps its existing text
layout and exposes each link as a real focusable accessible link. Clicking,
Tab/Shift+Tab, Enter and assistive activation all use `Desktop::open_link`.
Rendering, hovering or typing elsewhere never activates or fetches a link.
Timeline message selection and link navigation retain their existing path.

The interaction context shares a weak document lease and server/generation/key.
Before focus, reveal or activation it checks the current cached document, model
source and visible channel/profile. Replacing a document, disconnecting,
reconnecting, closing a profile or opening a modal over the conversation cannot
authorize an old callback through a still-resident cache entry. URLs and labels
share retained storage; callbacks do not copy raw server HTML.

Link rectangles come from the same wrapped line layouts used to draw text,
including formula offset mapping. The cache compares wrapped-layout identities
and line height, and rebuilds only after text layout changes. Focus nodes do not
contain duplicate label text or shape text. Focus notifications schedule one
ordinary next frame for reveal; scrolling requests another frame only when its
offset changes. Pointer hover refreshes only when the hovered link changes.

## Private native regression

Run the actual packaged executable and its sibling workers:

```sh
python3 tests/welcome_html.py \
  --binaries /path/to/package/bin \
  --murmur /path/to/mumble-server \
  --at-spi /path/to/at-spi2-core \
  --pulse-bin /path/to/pulseaudio/bin \
  --document-links --output /owned/cache/document-links
```

The fixture creates a disposable GNOME/Wayland and accessibility session, a
loopback stock Murmur, private profile, null-audio service and synthetic HTTPS
handler. The forced PulseAudio backend uses the private session runtime endpoint;
no host handler preferences, microphone, speaker or public server is used. The
handler only records its argument. `.test` URLs are never fetched.

The November 12, 2026 production package passed 16 expected URL activations across
welcome, channel-description and comment pointer/keyboard/assistive paths. Checks
also cover Tab order, wrapped Unicode, retained focus after width/200% zoom
changes, a pointer action at 200%, rejected drags/unsafe URLs, a normal-chat HTML
control, stale comment replacement, inactive documents behind a modal,
disconnect/reconnect target replacement and typing without activation. All 79
desktop unit tests passed, including the existing selection tests.

The description and comment fixtures intentionally stay below stock Murmur's
128-byte hash-only threshold. They establish delivered-document interaction;
the separate hash-only fetch/render gaps remain in TODO. Native Windows/macOS
execution is not established by this Linux GNOME run.

## Layout and idle evidence

A `frame-timing` build repeats the same scenario and records every frame and the
constant transport/audio input categories, with monotonic timestamps. The
fixture checks that focus-only navigation does not rebuild link geometry. After
three settling seconds with a document link focused, it observes the full next
eight seconds, requires at least one continuous two-second quiet interval and
rejects any geometry rebuild during that whole observation. Full timing logs and
idle-window timestamps are retained alongside screenshots and handler records.

A connected client still receives transport/latency notifications; the proof is
not a claim that the entire application never redraws while connected. Initial
failed fixtures intentionally lacked an owned audio endpoint and recorded the
existing media worker's two-second retry/status draws. A null-audio socket placed
outside the session runtime also remained invisible to the media sandbox. The
final fixture binds the socket at the actual private runtime path. Those failed
logs remain separate evidence and are not counted as document animation work.

The final instrumented Linux run recorded 156 draws: p95 request-layout was
0.814 ms and p95 total draw was 4.419 ms (maximum 6.895 ms). In its full 8.017-second
idle interval there were two transport notifications, exactly five seconds
apart, and two corresponding draws 0.043/0.046 ms later. There were no audio input
events or link-geometry rebuilds in that interval, and the longest continuous
quiet interval was 4.911 seconds. A resize after the observation drains the
collector's last pending draw while preserving that draw's original timestamp.
These are measurements of this private fixture, not a cross-platform benchmark.
