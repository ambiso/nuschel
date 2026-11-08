# Integration testing

Tests must launch their own servers on loopback or an isolated VM network. Do not
use public or third-party Mumble servers for testing. The host argument exists for
addressing the server fixture in another test VM.

Run `just vm` (Nix builds use `--cores 2`) for the Linux integration harness.
The test runs the actual GPUI client under Xvfb against a stock Mumble server,
with synthetic audio on a private PulseAudio daemon. It never opens the host microphone.
Screenshots are copied into the test result.

`mumble_client.py` uses the production session CLI to test certificate rejection/pinning,
login, channel trees and joining, permission denial, multiline chat and Opus tunneling.
It checks both codec directions against the reference libopus implementation.
`desktop_mumble.py` first checks an offline cached tree with long Unicode names,
expansion defaults, full-name tooltips, wheel scrolling and a draggable scrollbar.
It then drives the real Add Server dialog, certificate confirmation,
independent history browsing/double-click voice joining, chat, playback/capture, mute privacy,
audio service socket replacement, offline history and reconnect persistence.
The fixture seeds channels in a stopped stock server database; the real server handles
all protocol traffic. These tests do not yet drive the stock Mumble GUI or test UDP.

The VM also checks worker confinement, SQLite/PostgreSQL pagination and the NixOS
module's hardened database preflight. Nuschel server protocol tests remain deferred.
Both databases reopen the worker between pages and retain duplicate messages correctly.

## Client and server matrix

| Client | Server | Purpose |
| --- | --- | --- |
| Stock Mumble | Stock Mumble | Validate the stock fixture and virtual audio setup |
| Nuschel | Stock Mumble | Legacy-server compatibility and default local history |
| Stock Mumble | Nuschel | Legacy-client compatibility |
| Nuschel | Nuschel | Negotiated extensions and persistent server history |

Use a real stock Mumble GUI client with a deterministic X11 session and test profile;
do not replace it with a custom protocol script and call the result interoperability.
Start with distinct VM nodes, certificates, and fixture accounts. Test authentication,
channel changes, live multi-line chat, mute/deafen, permissions, and simultaneous users.

Once voice works, route deterministic PCM into virtual microphones and record the
receiving output. Check decoded content, latency, and packet loss handling. A client
showing a connected icon or an open UDP port is not proof of working voice. Exercise
UDP and forced TCP tunneling separately. Include channel/whisper routing isolation.

## Recovery and persistence

- Restart PipeWire, pipewire-pulse, and native PulseAudio during a session; network/chat
  stay connected and audio resumes without restarting the client.
- Remove/switch devices and change formats while processing is active.
- Add/remove an upstream AEC route and verify Nuschel does not stack another canceller.
- Snapshot unrelated graph objects and defaults before/after recovery.
- Restart client and server independently; compare legacy local history with negotiated
  server history and verify ACLs, observation timestamps, gaps, and repeated messages.
- Browse another channel/server while remaining in voice; deliver history responses out
  of order and ensure only the current conversation updates. Walk beyond the first
  page, append live messages concurrently, revoke access mid-pagination, and restart
  both peers. Entire retained, authorized history must remain reachable.
- For the future MLS-required mode, reject legacy clients and downgrade attempts.
  No green lock before authenticated MLS establishment; cover voice and chat separately.
  Exercise member/device removal, epochs, offline gaps, archive access grants and key
  loss. Assert that the server's stored archive never contains decrypted message bodies.
- Kill/stall content, media, and storage workers independently; verify bounded failure
  and the documented recovery behavior.

Run a separate NixOS VM sandbox test to establish forbidden file/network access. Keep
it separate from parser tests: successful IPC alone does not prove confinement.
Windows/macOS hardware, microphone permission, accessibility, Bluetooth, echo, and
mobile background-call behavior still need native platform testing.
