#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""User transitions, independent two-server state and native status presentation."""
import json
import signal
import time
from accessibility import items
from local_murmur import eventually
from mumble_client import Client


def run(args, root, shell, registry, bots, profile, second_port, second_pin, shot, murmur):
    assert registry and args.pulse_bin
    app = eventually(lambda: next((node for node in registry.children() if node.name.lower() == "nuschl"), None), "desktop absent")
    peer = Client(args.binaries, "127.0.0.1", second_port, second_pin, "Status observer", root / "status-observer")
    bots.append(peer)
    password = root / "status-test-password"
    password.write_text("owned-status-test")
    admin = Client(args.binaries, "127.0.0.1", second_port, second_pin, "SuperUser", root / "status-admin", ("--password-file", str(password)))
    bots.append(admin)
    names = {"alpha": "Voice Alpha", "beta": "Voice Beta"}
    records, failures = [], []
    def named(role, name, prefix=False):
        return next((node for node, r, text, _ in items(app) if (not role or r == role) and (text.startswith(name) if prefix else text == name)), None)
    def find(name, role="push button", prefix=False):
        return eventually(lambda: named(role, name, prefix), name + " absent")
    def own(which):
        observer, username = (bots[0], "Andi") if which == "alpha" else (peer, "Voice Beta client")
        return next((user for user in observer.users.values() if user["name"] == username), None)
    def point(node, button=1):
        g = json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return{x:b.x,y:b.y};})())"))
        x, y = node.centre(); shell.click(g["x"] + x, g["y"] + y, button=button)
    def select(which):
        find(names[which] + ";", prefix=True).press()
        eventually(lambda: find(names[which] + ";", prefix=True).call("Accessible", "GetState")[0][0] & (1 << 23), "text selection did not change")
    def state(which, status, channel=None):
        label = names[which] + (" #" + channel if channel else "") + ": " + status
        eventually(lambda: any(name == label for _, _, name, _ in items(app)), "voice state absent: " + label)
    def record(step):
        tree = list(items(app))
        links = [name for _, role, name, _ in tree if role == "push button" and name.startswith("Go to ")]
        link_bounds = [{"name": name, "bounds": node.extents()} for node, role, name, _ in tree if role == "push button" and name.startswith("Go to ")]
        assert all(value["bounds"][2] >= 32 and value["bounds"][3] >= 10 for value in link_bounds), link_bounds
        groups = [name for _, role, name, _ in tree if role in ("grouping", "section") and any(value in name for value in names.values())]
        value = {"step": step, "window_title": shell.window()["title"], "wire": {which: own(which) for which in names}, "voice_links": links, "link_bounds": link_bounds, "groups": groups, "visible_names": [name for _, _, name, _ in tree]}
        records.append(value); (args.output / "status-proof.json").write_text(json.dumps({"passed": not failures, "steps": records, "failures": failures}, indent=2) + "\n")
        shot("status-" + step)
        return value
    def setting(mode):
        find("Settings").press(); find("Settings", "dialog")
        control = find(mode, role=None); control.call("Component", "GrabFocus"); control.press()
        shell.key("Escape"); eventually(lambda: named("dialog", "Settings") is None, "settings stayed open")
    def context(which):
        point(find(names[which] + ";", prefix=True), button=3)
    def edit(which):
        context(which); find("Edit server…", "menu item").press()
        find("Add or connect to a Mumble server", "dialog")
    eventually(lambda: own("alpha"), "primary user absent")
    find("Unmute").press(); eventually(lambda: not own("alpha")["self_muted"], "primary unmute not acknowledged")
    select("beta"); find("Connect").press()
    eventually(lambda: own("beta"), "secondary login absent")
    if own("beta")["self_deafened"]:
        record("beta-initial-deafened")
        find("Undeafen").press(); eventually(lambda: not own("beta")["self_deafened"], "secondary undeafen not acknowledged")
    assert own("beta")["self_muted"] and not own("beta")["self_deafened"]
    initial = record("two-servers-ptt-idle")
    time.sleep(.5)
    alpha_packets = [event for event in bots[0].events if event["kind"] == "audio" and event["sender"] == own("alpha")["session"]]
    assert not alpha_packets, "push-to-talk idle unexpectedly transmitted"
    if any("Talking on" in name for name in initial["visible_names"]):
        failures.append("idle microphone destination is labeled Talking on with no transmitted packets")
    # Force a real ACK delay in the owned server, without network/worker state
    # injection. The local intent/paint must remain truthful until it resumes.
    murmur.process.send_signal(signal.SIGSTOP)
    try:
        find("Deafen").press()
        state("beta", "Deafened · awaiting server", "Root")
        assert not own("beta")["self_deafened"], "stopped server unexpectedly acknowledged"
        assert "Deafened · awaiting server" in find(names["beta"] + ";",prefix=True).name
        record("pending-deafen-real-ack-delay")
    finally:
        murmur.process.send_signal(signal.SIGCONT)
    eventually(lambda: own("beta")["self_deafened"] and own("beta")["self_muted"], "secondary deafen not acknowledged")
    deaf = record("secondary-deafened")
    if not any(names["beta"] in label for label in deaf["voice_links"]):
        failures.append("secondary deafened connected server has no voice destination row")
    find("Unmute").press(); find("Speak on two servers?", "dialog"); find("Cancel").press()
    assert own("beta")["self_deafened"] and not own("alpha")["self_muted"]
    record("consent-cancel")
    find("Unmute").press(); find("Speak on two servers?", "dialog"); find("Only on " + names["beta"]).press()
    eventually(lambda: own("alpha")["self_muted"] and not own("beta")["self_muted"] and not own("beta")["self_deafened"], "destination move not acknowledged")
    record("destination-swapped")
    setting("Continuous")
    eventually(lambda: any(event["kind"] == "audio" and event["sender"] == own("beta")["session"] and not event["end"] for event in peer.events), "synthetic continuous audio absent")
    state("beta", "Speaking", "Root")
    record("actual-audio-sending")
    admin.send(kind="moderate", session=own("beta")["session"], action="mute")
    eventually(lambda: own("beta")["muted"], "server mute not observed")
    state("beta", "Microphone destination · server muted", "Root"); record("server-muted")
    admin.send(kind="moderate", session=own("beta")["session"], action="deafen")
    eventually(lambda: own("beta")["deafened"], "server deafen not observed")
    state("beta", "Deafened by server", "Root"); record("server-deafened")
    admin.send(kind="moderate", session=own("beta")["session"], action="undeafen")
    admin.send(kind="moderate", session=own("beta")["session"], action="unmute")
    eventually(lambda: not own("beta")["deafened"] and not own("beta")["muted"], "server voice restrictions not cleared")
    setting("Push to talk"); time.sleep(.6); record("ptt-idle-again")
    # Rapid gestures target the same control; deafen must always imply mute.
    control = find("Deafen"); x, y = control.centre()
    g = json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return{x:b.x,y:b.y};})())"))
    shell.click(g["x"] + x, g["y"] + y, count=8)
    time.sleep(.5); record("rapid-deafen-observed")
    eventually(lambda: own("beta")["self_muted"] and not own("beta")["self_deafened"], "rapid final intent not acknowledged")
    record("rapid-deafen-final")
    find("Unmute").press(); eventually(lambda: not own("beta")["self_muted"], "unmute after rapid gestures failed")
    find("Deafen").press(); eventually(lambda: own("beta")["self_deafened"], "primary deafen failed")
    select("alpha"); record("alpha-text-beta-deafened")
    select("beta"); find("Undeafen").press(); eventually(lambda: not own("beta")["self_deafened"], "undeafen failed")
    find("Unmute").press(); eventually(lambda: not own("beta")["self_muted"], "primary unmute failed")
    channel = find("Lobby", role=None)
    # A real double click is generated with one device gesture.
    g = json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return{x:b.x,y:b.y};})())")); cx, cy = channel.centre(); shell.click(g["x"] + cx, g["y"] + cy, count=2)
    eventually(lambda: own("beta")["channel"] == 1, "voice channel move did not arrive")
    record("channel-moved")
    edit("beta"); entry = find("Server name", "entry"); entry.call("Component", "GrabFocus"); shell.chord("Control_L", "a"); shell.type("Voice Beta renamed"); find("Save").press(); names["beta"] = "Voice Beta renamed"
    eventually(lambda: named("push button", names["beta"] + ";", True), "renamed server absent")
    record("server-renamed")
    # A prompt's answer cannot attach to a reconnected presence, even if the
    # backend retains the outer generation and the server reuses session IDs.
    select("alpha"); find("Unmute").press(); find("Speak on two servers?", "dialog")
    murmur.stop()
    eventually(lambda: "Connecting" in find(names["beta"] + ";", prefix=True).name, "lost server did not enter reconnect")
    state("beta", "Reconnecting"); record("reconnecting-no-stale-channel")
    find("Only on " + names["alpha"]).press()
    eventually(lambda: named("dialog", "Speak on two servers?") is None, "stale prompt not dismissed")
    assert own("alpha")["self_muted"], "stale multi-server consent unmuted Alpha"
    record("stale-consent-rejected")
    murmur.start()
    peer = Client(args.binaries, "127.0.0.1", second_port, second_pin, "Status observer restarted", root / "status-observer-restarted")
    bots.append(peer)
    eventually(lambda: own("beta"), "secondary reconnect absent")
    eventually(lambda: "Connected" in find(names["beta"] + ";", prefix=True).name, "secondary UI did not reconnect")
    record("reconnected-current-membership")
    select("beta")
    if own("beta")["self_deafened"]:
        find("Undeafen").press(); eventually(lambda: not own("beta")["self_deafened"], "reconnect undeafen failed")
    if own("beta")["self_muted"]:
        find("Unmute").press(); eventually(lambda: not own("beta")["self_muted"], "reconnect explicit unmute failed")
    point(find("Offline earlier;", prefix=True), button=3); find("Edit server…", "menu item").press(); find("Remove saved server").press()
    select("alpha"); find("Go to " + names["beta"], prefix=True).press()
    eventually(lambda: find(names["beta"] + ";", prefix=True).call("Accessible", "GetState")[0][0] & (1 << 23), "voice row navigated through stale runtime index")
    record("earlier-server-removed")
    def inside(box, outer):
        x,y,w,h=box;ox,oy,ow,oh=outer
        return w > 0 and h > 0 and x >= ox-1 and y >= oy-1 and x+w <= ox+ow+1 and y+h <= oy+oh+1
    select("beta"); edit("beta")
    entry=find("Server name","entry"); entry.call("Component","GrabFocus")
    shell.chord("Control_L","a")
    names["beta"] = "Voice Beta " + "Überraschung " * 7
    # The private shell's synthetic keyboard does not implement an IME.
    # Paste the synthetic Unicode label from its OWN clipboard, never the host.
    shell.eval("imports.gi.St.Clipboard.get_default().set_text(imports.gi.St.ClipboardType.CLIPBOARD," + json.dumps(names["beta"]) + "); 'ok'")
    shell.chord("Control_L","v"); time.sleep(.2)
    find("Save").press()
    (args.output/"long-name-bookmarks.json").write_text((profile/"config/servers.json").read_text())
    shot("status-long-name-save")
    names["beta"] = names["beta"].strip()
    eventually(lambda: named("push button", names["beta"] + ";",True), "long Unicode name absent")
    record("long-unicode-name")
    geometry=[]
    for width,height in [(1280,800),(860,520)]:
        shell.place("Nuschl", 20, 20, width, height)
        eventually(lambda: shell.window()["width"] == width and shell.window()["height"] == height, "native resize not committed")
        for percent,steps in [(100,0),(125,2),(150,3),(200,5)]:
            shell.chord("Control_L","0")
            for _ in range(steps): shell.chord("Control_L","equal"); time.sleep(.12)
            eventually(lambda: abs(json.loads((profile/"config/settings.json").read_text())["appearance"]["zoom"] - percent/100) < .001, "native zoom not persisted")
            time.sleep(.35)
            for which in ["beta", "alpha"]:
                link=find("Go to " + names[which],prefix=True)
                link.call("Component","GrabFocus")
                region=find("Voice destinations",role=None)
                eventually(lambda: inside(link.extents(),region.extents()), "voice name not visibly revealed in bounded lane")
                action=find("Disconnect from " + names[which] + " (voice and chat)")
                action.call("Component","GrabFocus")
                eventually(lambda: inside(action.extents(),region.extents()), "targeted hang-up not visibly revealed")
                assert inside(link.extents(),region.extents()), "hang-up focus hid the server name"
                geometry.append({"size":[width,height],"zoom":percent,"server":which,"name":link.extents(),"action":action.extents(),"region":region.extents()})
            (args.output/"status-layout.json").write_text(json.dumps(geometry,indent=2)+"\n")
            shot(f"status-layout-{width}-{percent}")
    shell.chord("Control_L","0"); shell.place("Nuschl",20,20,1280,800); time.sleep(.4)
    # End muted on both, then prove the hang-up scope independently of text selection.
    find("Mute").press(); eventually(lambda: own("beta")["self_muted"], "final mute failed")
    select("alpha")
    targeted = named("push button", "Disconnect from " + names["beta"] + " (voice and chat)")
    if targeted:
        targeted.press(); eventually(lambda: own("beta") is None and own("alpha") is not None, "targeted disconnect affected another server")
    else:
        find("Disconnect").press(); eventually(lambda: own("alpha") is None and own("beta") is not None, "baseline disconnect did not follow selected text server")
        failures.append("voice-card header disconnects selected Alpha text context while microphone destination is Beta")
    record("disconnect-scope")
    assert not failures, failures
    print("Native voice status transitions passed", flush=True)
