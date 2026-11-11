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
all protocol traffic.

## Suites in the VM run

| Script | What it proves |
| --- | --- |
| `udp_voice.py` | Encrypted OCB2 UDP voice, forced TCP and mixed relay; jitter and 5% loss with netem; inbound UDP blocked (IPv4 and IPv6) falls back to the TLS tunnel and recovers |
| `protocol_features.py` | Whisper/shout isolation, channel listening, permissions, user stats, comments and avatars, self-registration, SuperUser administration (registered users, bans, ACL, kick), OpenSSL-made PKCS#12 import (channel links are not yet covered) |
| `media_live.py` | Live media worker on a private PulseAudio: jitter-buffer continuity, reuse of a system echo canceller with playback routed to its reference, echo modes and overrides, the Settings echo test at -31 dB coupling with and without Nuschel's canceller, clipping detection that leaves system levels untouched |
| `pipewire_recovery.py` | A private PipeWire + WirePlumber + pipewire-pulse session with virtual nodes only: playback, PipeWire restart with the replacement worker pinned to PipeWire (no ALSA fallback), pipewire-pulse restart, PipeWire echo-cancel reuse without touching session defaults, device lists without other applications' streams, an RNNoise filter-chain behind echo-cancel (both of Nuschel's stages off, playback to the canceller) |
| `opus_eval.py` | opus-pure against reference libopus: decoder conformance in SILK, Hybrid and CELT, encoder interop both ways, 5,000 hostile packets |
| `stock_client.py` | The real stock Mumble GUI with a certificate exported by Nuschel; voice in both directions |
| `desktop_recovery.py` | Media and storage worker crashes keep the session and history; server restart reconnects with backoff, rejoins the channel muted and marks the gap |
| `accessibility.py` | The AT-SPI tree as a screen reader sees it: named controls, channel tree, users and composer; Mute operated through the Action interface alone; every message of a long conversation is a list item, the oldest (screens above the viewport) scrolled into view by AT-SPI ScrollTo and GrabFocus |
| 1.4 / 1.3 loops | `mumble_client.py`, `udp_voice.py` and `protocol_features.py` against stock Mumble 1.4.287 and 1.3.4 (legacy UDP format) |

Development aids outside the VM: `screenshots.py` (scenes under a private headless
GNOME Shell on the real GPU; it never touches the user's session) and
`accessibility.py --gnome-shell` (the same check with Nuschel's own window controls,
and the timeline walked with the keyboard: Shift+Tab, arrows, Home and End, Tab through
a message's links, a letter typed into the composer). Against a loopback `nuscheld`:
`with_local_nuscheld.py <binaries> nuschel_conversations.py <binaries> --port PORT`
(edits, deletions and their authorisation, typing, direct conversations, with
`nuschel-cli`) and `conversation_screenshots.py` (the desktop with edits, tombstones,
the typing line, a direct conversation from server history, keyboard links, and the
local cache it leaves, in a private headless GNOME Shell).

`windows_vm.py` installs Windows 11 Enterprise (evaluation ISO from Microsoft)
unattended into a KVM guest with two vCPUs and drives it through a PowerShell agent
in the desktop session, QMP screenshots, clicks and keys. The guest has no network
except forwards into host loopback (the harness on 10.0.2.100:80, a test Murmur on
10.0.2.100:64738). Keep its state directory (`$NUSCHEL_WINDOWS_VM_DIR`, default
`~/vms/nuschel-windows`) out of backups and off a RAM-backed `/tmp`.

### Windows worker sandbox

`crates/sandbox-windows/tests/confinement.rs` checks every worker's AppContainer from
the inside (docs/windows-sandbox.md). Cross-build it with the workers
(`cargo test --target x86_64-pc-windows-gnu -p nuschel-sandbox-windows --test
confinement --no-run`), copy the test executable and the five worker executables
into one folder in the guest (`windows_vm.py push`, then `Invoke-WebRequest`), and
run `confinement.exe --workers <folder>` there. It starts itself through
`nuschel_ipc::host`'s real commands as each worker kind; inside, it must fail to read
a file it creates in Documents, list or write the profile, `C:\Users\Public` or the
user's TEMP, connect to 10.0.2.100:80 (only transfer may), start a process, open the
test's own process to query or terminate it, commit more memory than its job
allows, read the clipboard (the test puts text there first) or the autostart key, or
create a registry key; content, storage and crypto (less privileged AppContainers)
must also fail to read `HKCU\Environment`, which media and transfer can (printed, not
failed). Storage and crypto must write their directory and transfer must read its
offered file by handle. Then the real workers run in the same sandboxes: content decodes a
PNG, storage writes and reads history, crypto creates an identity, media opens the
default WASAPI devices (the VM's sound card has no host backend), transfer offers a
file. For a full transfer, start the VM with `--forward 4710 --forward 4711`, run
`twist wormhole-mailbox --port=tcp:4710:interface=127.0.0.1` and `twist transitrelay
--port=tcp:4711:interface=127.0.0.1` on the host, and add `--rendezvous
ws://10.0.2.100:4710/v1 --relay tcp://10.0.2.100:4711` plus `--code-file PATH`
(the host receives with the reference CLI and the code written there) or `--receive
CODE --expect TEXT` (the host sends). It exits non-zero if any check fails.

Result on 2026-11-11 (Windows 11 25H2): all checks passed for every worker kind,
including both transfer directions; the desktop, run in the VM against a stock Murmur
on host loopback, rendered an image message, stored history and captured from the
microphone with its workers in AppContainers.

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
Windows/macOS hardware, microphone permission, screen readers (NVDA, Narrator,
VoiceOver), Bluetooth, echo, and mobile background-call behavior still need native
platform testing. The macOS Seatbelt profiles have a macOS-only test that has not run;
the Windows sandbox has run in the Windows 11 VM only (above).

## WebRTC transport trial

`cargo test -p nuschel-rtc` exchanges ICE/DTLS/SRTP datagrams in memory with a virtual
clock. It checks bidirectional Opus/VP8 payloads and timestamps, video fragmentation
and retransmission, audio progress during video loss, video backpressure, SDP/media
limits, shutdown, fingerprint rejection, authenticated-packet tampering and replay.
The harness drops an interior video packet; a missing first-ever RTP packet has no
previous sequence number from which a receiver can infer the loss.

`just rtc` builds the real `nuschel-rtc-probe` and media worker. `rtc_probe.py` sends
a generated tone through the worker encoder, the two loopback peers and the worker
decoder. The VM also decodes the received packet with reference libopus. Video uses
the locally generated [VP8 keyframe](fixtures/README.md), spans multiple RTP packets,
and is compared byte for byte after reassembly. The probe fails if encoded-frame
delivery accumulates over 500 ms of delay; that threshold accommodates a loaded test
machine and is not an acceptable target for live voice quality. No host audio devices
are opened. Codec helpers are separate processes; the direct diagnostic invocation
does not establish their OS confinement.

These are transport/codec checks, not live browser, camera, renderer, A/V sync or
long-call quality tests. The current SDP adapter supports an initial audio-only or
audio/video offer/answer, one track per kind; renegotiation, ICE restart, SFU forwarding
and server capability negotiation are not implemented. SDP fingerprints in the probe
are exchanged in-process; a real call must authenticate that signaling channel.
