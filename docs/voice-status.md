# Native voice status

The native card describes server-scoped voice intent and observed activity. It is
independent of the server currently selected for text. It does not change the
multi-server routing policy described in [clients.md](clients.md#voice-on-several-servers).

Each connected server, plus an actual connection/reconnection attempt, has one row.
Offline saved bookmarks do not fill the card. The primary line names the server
and its acknowledged voice channel. During reconnect it names only the server;
old cached membership is not presented as current. The fixed hang-up action names
that server and explicitly disconnects **voice and chat**. Selecting the destination
line browses its current channel without moving microphone ownership.

One pure classification supplies the rail, card, selected-server footer and current
voice-channel heading. Microphone destination means routing intent, not speech or
hardware readiness. An idle unmuted push-to-talk destination is not called Talking.
Speaking requires current own-user activity and is suppressed immediately by local
mute/deafen/push-to-mute, server mute/deafen/suppression, missing acknowledgement,
unsupported/unknown codec, E2EE voice unavailability or insufficient transmit budget.
A listening server remains a listening context even when another destination is
deafened. All connected deafened contexts remain visible.

Local privacy choices take effect immediately. If the authenticated own-user state
has not acknowledged them, the status also says awaiting server. This distinction
is important when a stock server drops rapid controls under its shared message
bucket. Audio-device detail remains available in the footer tooltip, and each
server's reported health issue opens audio settings for that server. The fixed
signal glyph is green only when the projection has a supported voice context.
Chat/TLS alone, an unknown codec and unsupported/E2EE-only voice cannot produce a
green Voice connected heading. It describes supported membership, not device
readiness, active capture or a measured quality score.

Rail tiles, their focus identities, navigation/context/keyboard callbacks and voice
rows use stable server identity rather than a runtime vector position. Row bindings
include generation, saved endpoint and own presence. Navigation and disconnect
actions resolve the named connection again. Actions that enable audio also validate
captured own presence and acknowledged channel lifetime. Privacy stops resolve the
current named ID/generation/endpoint even if an automatic reconnect replaced its
own presence; they do not wait for the old presence. The second-server question captures the shown
places and microphone destination/active sets. Approval is rejected if those facts
changed; cancellation never unmutes anything. An auto-reconnect can preserve the
outer generation, so own presence and channel instances are checked too.

The card uses the common bounded scroll region and focus-reveal behavior. A label
has intrinsic height inside its vertical lane, a finite horizontal text lane and
full accessible name/tooltip; hang-up actions occupy fixed slots. The per-render
place collection is reused by the card and footer. Channel-name and channel-binding
resolution share the existing single lookup per relevant server. Action-time
validation does not add text measurement or per-frame channel scans.

## Verification and limits

`tests/voice_status.py` is an actual native GNOME/AT-SPI scene in
`tests/screenshots.py --scenes voice-status`. It uses two owned loopback stock
Murmurs, independent CLI observers, an owned private Pulse null sink and synthetic
microphone. It never uses a host audio socket or profile. The test covers PTT idle
versus real encoded audio, secondary deafen, consent cancel/move, server-enforced
mute/deafen, rapid controls, text/voice selection divergence, channel/name changes,
auto-reconnect, stale consent rejection, removal of an earlier runtime index and
targeted disconnect. A pre-removal accessible rail object must still select the same
saved server after a completed render; keyboard selection and persisted context
edits check the same identity. A briefly stopped **owned** Murmur holds a real deafen
ACK, then resumes in a finally block, proving the pending badge/text. A separate
owned TLS/framing fixture omits CodecVersion initially, then advertises false/true
Opus values to exercise negotiating/unavailable/supported headings. That fixture
does not establish codec/media interoperability. Positive geometry and focus checks cover narrow/default
windows, real native zoom, long Unicode names and visible label/action bounds.

The original cached package reproduced three semantic failures: idle PTT was called
Talking on; secondary deafened contexts disappeared; the card's generic Disconnect
acted on the text selection rather than the displayed microphone destination.
Names and channels did update through rename and move in that controlled run: no
literal stale text-cache failure was established. A first new-layout trial exposed
zero-height labels despite accessible names; that negative screenshot is retained
and the fixture now requires positive painted geometry.

The initial rapid-control trial retained stock message-burst defaults and failed to
acknowledge the final state. The deterministic presentation matrix raises **only its
owned test servers'** control bucket to isolate client projection. That does not
prove default-rate reconciliation; the retained default-bucket failure remains a
separate investigation. Local mute/deafen intent and remote acknowledgement must
not be conflated.

UI validation is not an actor-level `SetSelf` fence. The existing `ActionOrigin`
envelope does not support `SetSelf` (`Command::targets()` returns `None`), so a
connection/context change between the UI check and actor execution remains a
separate authority follow-up. Cross-server privacy stops are not routed through an
unrelated server scope. No hardware-readiness, physical microphone or full network
reliability guarantee follows from these private fixtures.

The frozen source passed 96 desktop unit tests. The final private GUI run passed
23 recorded state transitions and 16 positive name/action geometry checks across
1280×800 and 860×520 at 100/125/150/200%, including the long Unicode name. The
original two-server audio/privacy suite also passed on that exact native source,
including independent playback, PTT-only destinations, consent, privacy stops and
media-worker recovery. Runtime proof uses the exact native leaf with cached
packaged sibling helpers; root distribution integration remains a separate gate.
