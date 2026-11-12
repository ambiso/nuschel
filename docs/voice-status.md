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
signal glyph means connection status; it is not a measured quality score.

Rows and focus identities use stable server ID, generation and own presence rather
than a runtime vector position. Navigation and disconnect actions resolve the named
connection again. Actions that enable audio also validate the captured own presence
and acknowledged channel lifetime. The second-server question captures the shown
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
targeted disconnect. Positive geometry and focus checks cover narrow/default
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
