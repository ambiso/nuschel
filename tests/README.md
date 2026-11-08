# Integration testing

The NixOS VM test driver is the Linux integration harness. The foundation smoke test
checks installed Nuschel binaries, a real stock Mumble server, and worker processing.
It does not claim client/server protocol compatibility while sessions are unimplemented.
It also provisions PostgreSQL through the Nuschel NixOS module, runs its real migration
preflight under the hardened service settings, and exercises persistent SQLx operations.
Both SQLite and PostgreSQL tests reopen the storage worker between archive pages and
verify that identical messages/timestamps survive pagination without gaps or duplicates.

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
