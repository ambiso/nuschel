# Emphasized domains in welcome notices

The November 12 report was `<b>something.de</b>` appearing as
`bsomething.de/b` after joining a server. The current native package
`/nix/store/462ayz1f9900zn9iwwjxw71dax28xfna-nuschl-0.1.0`, from root
`95a1a27f`, passes the exact-domain check against owned loopback stock Murmur
inside private GNOME/null Pulse. The worker returns visible `something.de` with
a bold span covering bytes 0–12. The desktop shows that text without tag names
or storage markers. No new native production fix is claimed: this is precise
regression evidence for the existing typed welcome-document path.

`tests/welcome_domain.py` additionally checks strong emphasis, an explicit HTTPS
URL inside bold, a bold explicit anchor, escaped literal tags, multiline text
and a Rust code fence. Remote images/scripts remain inert and no public URL is
activated. The existing welcome harness is unchanged. The first fixture trial
stopped before GUI startup because its escaped literal `<b>` control conflicted
with that harness's broad assertion; the final GUI control uses escaped `<em>`
and the worker samples retain the literal `<b>` case.

Evidence is retained in
`~/.cache/nuschl-next/welcome-domain/native-current2/`: `proof.json`,
`domain-worker.json`, `domain-visible.txt` and `domain-welcome.png`. The screenshot
was visually reviewed. Worker-only initial samples against the earlier native
bundle are separate in `worker-initial.json`.

## Browser implementation

The browser now forwards `ServerSync.welcome_text` after the authenticated
connection event. The existing memory-safe legacy projection and document
renderer run in the SDK WebAssembly Worker, with image codecs disabled. The
page receives typed blocks, styled spans and explicitly activated safe links;
it never interprets the welcome as DOM HTML or retrieves embedded resources.
Both standalone and linked emphasis retain their styles. The chat and welcome
surfaces share `TypedDocument.svelte`, including code highlighting/copy, literal
newlines, Show more and the existing isolated formula renderer. This does not
activate the still-pending attachment/content broker.

Sources are limited to 256 KiB before parsing; oversized input becomes a plain
display-limit notice. Each rendered document is limited to 256 KiB of serialized
JSON, with a plain fallback if markup expansion exceeds that bound. The worker
cache admits at most 16 entries and 2 MiB of source plus serialized-document
payload in total, evicting older entries. These are retained payload budgets,
not claims about exact allocator overhead or parser peak memory. The shared
client's existing raw welcome state and event bounds remain separate.

Every cache entry binds the stable server ID, exact original bounded source and
connection generation. Shared-client acceptance checks those same live
authorities. Documents cross the Worker boundary once in an accepted update;
normal server snapshots contain only an opaque token. The App drops a document
when its server disconnects, becomes unavailable, is removed/replaced, or its
token changes. Dismiss validates the current connection/source/token, so an old
click cannot hide a new welcome. Dismissal lasts for that source/connection;
reconnecting shows the welcome again. It is not a permanent preference.

The panel has a viewport-height budget, a keyboard-scrollable body and a fixed
Dismiss action. Long server titles use a two-line visual bound while retaining
the full accessible text and tooltip. Phones retain the chat-first layout and a
reachable composer.

`tests/web_welcome.py` launches owned loopback Murmur and relay processes; its
browser companion exercises the actual compiled SDK and App under the
production CSP. It covers the exact bold domain, linked bold domain, escaped
literal tags, blank lines, highlighted code, same-origin/remote image and script
canaries, single document delivery, keyboard scroll/dismiss, reconnect, bookmark
replacement, old-token rejection and retained drafts. The 320/390/900/1280
viewport checks and long-name screenshots are synthetic Chromium checks, not
physical Safari/mobile keyboard or assistive-technology certification.

Live `ServerConfig.welcome_text` updates remain a distinct queued gap: both
native and browser handlers currently discard that optional field, whereas
stock Mumble's `MainWindow::msgServerConfig` logs nonempty values. These checks
cover initial `ServerSync` delivery, not live configuration updates.

## Appearance preference regression

This work also exposed a separate central font-size defect: Settings wrote
`--font-size`, but only `body` used it. At a selected 20px, the root remained
16px and a `.83rem` welcome paragraph remained 13.28px. The separate fix applies
the same variable to `html`, so rem-sized controls/messages follow the choice.
The real App fixture checks root sizes 14/16/20px, actual message text
12.6/14.4/18px, persistence after reload, and largest-setting phone/desktop
bounds. An additional explicitly labeled 32px root-font stress verifies the
welcome action remains visible; this is not physical browser zoom evidence.

Evidence and reproducible invocations are in
`~/.cache/nuschl-web-welcome/handoff.json`. Retained initial failures distinguish
a missing fixture scratch directory, the drawer's automatic close behavior and
a responsive-transition sampling race from production regressions. The
pre-fix computed-font measurements are retained separately.
