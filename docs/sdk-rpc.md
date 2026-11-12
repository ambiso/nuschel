# Browser SDK request ownership

A stalled SDK worker now makes the client unavailable without recycling its
pending request slots into the same unresponsive inbox. `sdk-rpc.js` owns at most
128 unresolved commands and monotonic safe-integer request IDs. Only a matching
result releases an active owner's slot. Duplicate/unknown results do not settle
another request. A normal command error remains retryable and does not kill the
worker.

A scheduled 15-second request deadline, worker error or post failure permanently
closes that owner. It requests `Worker.terminate()`, revokes the current audio
route/consent and then rejects pending promises. All callbacks check the owned
worker identity; late ready/state/result events cannot revive it. No automatic
worker replacement or microphone reacquisition occurs. If `terminate()` throws,
the lane still closes and audio is revoked, but pending requests are retained
without synthetic rejection or credit release.

The UI shows **Unavailable** and an explicit **Reload client** action. Voice and
talking indicators stop; sending is disabled. Existing conversation text,
bookmarks, certificate pins and history remain. The current draft remains in a
focusable read-only textarea for selection/copy. It is not automatically submitted
or silently cleared. Drafts still live in page memory: copy an unsent draft before
explicitly reloading; this change does not add draft persistence.

BrowserAudio's two acknowledged SDK audio-send slots are global across TX epoch
changes. Its 250 ms capture deadline closes capture but does not acknowledge the
SDK inbox or release those slots. A real result, or a synthetic rejection from
an already permanently retired SDK owner, settles them. Old completion identity
checks remain, and a duplicate cannot consume a later slot.

## Logical retirement and physical shutdown

`Worker.terminate()` returns no physical-exit acknowledgment. The
[WHATWG termination algorithm](https://html.spec.whatwg.org/multipage/workers.html#terminate-a-worker)
runs in parallel with the worker. In the confined Chromium156 fixture, termination
was requested at the 15-second deadline, but a deliberately busy ordinary worker
task still emitted its 17-second console tail before native close was observed.
Chromium's [upstream worker implementation at an immutable revision](https://chromium.googlesource.com/chromium/src/+/7955b5259674583a28ee817ed9c0562449774052/third_party/blink/renderer/core/workers/worker_thread.cc)
has a two-second forcible-termination grace and debugger-related postponement;
that is consistent with the measured lag, not proof of this binary's exact path.

The guarantee is permanent logical retirement before synthetic settlement:
no new commands are posted to that owner, no results are accepted from it, and
no replacement is automatically spawned. Thus its inbox cannot keep growing while
native shutdown completes. This does **not** guarantee synchronous thread death
or prevent delivery of previously submitted network traffic. Browser timer
scheduling is also not a hard real-time deadline. Future automatic replacement
needs its own aggregate ownership/resource bound; it cannot infer worker exit
from the void termination API.

## Verification

`node --test tests/sdk_rpc.unit.mjs tests/web_audio.unit.mjs tests/web_playout_clock.mjs`
passes 36 checks. The six focused RPC checks independently exercise the 128-command
cap, exact acknowledgments, termination-before-synthetic-settlement ordering,
transient errors, failed termination, and two audio slots through 20 epochs plus
the 250 ms timeout.

`tests/sdk_rpc.browser.mjs APP RELAY_URL MURMUR_PORT WRAPPER OUTPUT` runs an actual
packaged App under production CSP on a private server. The fixture appends only
an ordinary-message busy-task listener to the served SDK module; the original
packaged SDK prefix remains unmodified. A 17-second busy loop blocks processing.
After the two real audio sends fill, 140 actual UI click events fill the aggregate
inbox to exactly 128. The test observes retirement, ended synthetic microphone
tracks, unavailable/no-voice UI, a selectable preserved draft, unchanged browser
bookmarks/certificate/history, and no new posts or application results through
the later worker tail. Explicit stale ready/state/result callbacks must also fail
to restore the old owner. No host device, profile, credential or public server is
used. A separate normal-call proof serves the SDK without fault injection.

All three original synchronous-death assertions are retained as failed evidence
under `~/.cache/nuschl-sdk-rpc/browser-{inspector,timer,message}-failure/`.
The accepted logical-retirement scope does not overwrite them or relabel them as
passing. Final proofs and source/artifact hashes are recorded in
`~/.cache/nuschl-sdk-rpc/handoff.json`; ordinary calling/text/order/privacy and
production-CSP regressions are separate from this injected-stall fixture.
