#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""User transitions, independent two-server state and native status presentation."""
import json
import time
from accessibility import items
from local_murmur import eventually
from mumble_client import Client


def run(args, root, shell, registry, bots, profile, second_port, second_pin, shot):
    assert registry and args.pulse_bin
    app = eventually(lambda: next((node for node in registry.children() if node.name.lower() == "nuschl"), None), "desktop absent")
    peer = Client(args.binaries, "127.0.0.1", second_port, second_pin, "Status observer", root / "status-observer")
    bots.append(peer)
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
        eventually(lambda: names[which] in shell.window()["title"], "text selection did not change")
    def record(step):
        tree = list(items(app))
        links = [name for _, role, name, _ in tree if role == "push button" and name.startswith("Go to ")]
        groups = [name for _, role, name, _ in tree if role in ("grouping", "section") and any(value in name for value in names.values())]
        value = {"step": step, "window_title": shell.window()["title"], "wire": {which: own(which) for which in names}, "voice_links": links, "groups": groups, "visible_names": [name for _, _, name, _ in tree]}
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
    assert own("beta")["self_muted"] and not own("beta")["self_deafened"]
    initial = record("two-servers-ptt-idle")
    time.sleep(.5)
    alpha_packets = [event for event in bots[0].events if event["kind"] == "audio" and event["sender"] == own("alpha")["session"]]
    assert not alpha_packets, "push-to-talk idle unexpectedly transmitted"
    if any("Talking on" in name for name in initial["visible_names"]):
        failures.append("idle microphone destination is labeled Talking on with no transmitted packets")
    find("Deafen").press(); eventually(lambda: own("beta")["self_deafened"] and own("beta")["self_muted"], "secondary deafen not acknowledged")
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
    record("actual-audio-sending")
    setting("Push to talk"); time.sleep(.6); record("ptt-idle-again")
    # Rapid gestures target the same control; deafen must always imply mute.
    control = find("Deafen"); x, y = control.centre()
    g = json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return{x:b.x,y:b.y};})())"))
    shell.click(g["x"] + x, g["y"] + y, count=8)
    eventually(lambda: own("beta")["self_muted"] and not own("beta")["self_deafened"], "rapid final intent not acknowledged")
    record("rapid-deafen-final")
    find("Unmute").press(); eventually(lambda: not own("beta")["self_muted"], "unmute after rapid gestures failed")
    find("Deafen").press(); eventually(lambda: own("beta")["self_deafened"], "primary deafen failed")
    select("alpha"); record("alpha-text-beta-deafened")
    select("beta"); find("Undeafen").press(); eventually(lambda: not own("beta")["self_deafened"], "undeafen failed")
    find("Unmute").press(); eventually(lambda: not own("beta")["self_muted"], "primary unmute failed")
    channel = find("Lobby", role=None)
    point(channel); point(channel)
    # A real double click is generated with one device gesture.
    point(channel); g = json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return{x:b.x,y:b.y};})())")); cx, cy = channel.centre(); shell.click(g["x"] + cx, g["y"] + cy, count=2)
    eventually(lambda: own("beta")["channel"] == 1, "voice channel move did not arrive")
    record("channel-moved")
    edit("beta"); entry = find("Server name", "entry"); entry.call("Component", "GrabFocus"); shell.chord("Control_L", "a"); shell.type("Voice Beta renamed"); find("Save").press(); names["beta"] = "Voice Beta renamed"
    eventually(lambda: named("push button", names["beta"] + ";", True), "renamed server absent")
    record("server-renamed")
    point(find("Offline earlier;", prefix=True), button=3); find("Edit server…", "menu item").press(); find("Remove saved server").press()
    select("alpha"); find("Go to " + names["beta"], prefix=True).press()
    eventually(lambda: names["beta"] in shell.window()["title"], "voice row navigated through stale runtime index")
    record("earlier-server-removed")
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
