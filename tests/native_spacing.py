#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Real private-GNOME rail/footer spacing and bounded pointer activation."""
import json
import time
from accessibility import items
from layout_regression import inside
from local_murmur import eventually


def run(args, shell, registry, bots, profile, shot):
    assert args.pulse_bin and registry, "native-spacing needs owned null Pulse and AT-SPI"
    app = eventually(lambda: next((n for n in registry.children() if n.name.lower() == "nuschl"), None), "desktop absent")
    def named(role, name, prefix=False):
        return next((n for n, r, text, _ in items(app) if (not role or role == r) and (text.startswith(name) if prefix else text == name)), None)
    def find(name):
        return eventually(lambda: named("push button", name), name + " absent")
    def geometry():
        return json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const f=w.get_frame_rect(),b=w.get_buffer_rect();return {frame:{x:f.x,y:f.y,width:f.width,height:f.height},buffer:{x:b.x,y:b.y,width:b.width,height:b.height}};})())"))
    def pointer(node):
        g = geometry(); x, y = node.centre()
        shell.click(g["buffer"]["x"] + x, g["buffer"]["y"] + y)
    def own():
        return next((user for user in bots[0].users.values() if user["name"] == "Andi"), None)
    eventually(own, "owned server did not observe desktop")
    if named("push button", "Undeafen"):
        find("Undeafen").press()
        eventually(lambda: not own()["self_deafened"], "initial undeafen not acknowledged")
    assert own()["self_muted"], "geometry starts with microphone muted"
    first = eventually(lambda: named("push button", "Screenshot server;", True), "first server absent")
    last = eventually(lambda: named("push button", "Rail last;", True), "last server absent")
    records = []
    proof_file = args.output / "spacing-proof.json"
    def save():
        proof_file.write_text(json.dumps({"cases": records, "capture_backend": "owned null Pulse only"}, indent=2) + "\n")
    for label, width, height in [("narrow-small", 900, 500), ("default", 1280, 800), ("wide", 1600, 900), ("short", 1200, 480)]:
        for percent, steps in [(100, 0), (125, 2), (150, 3), (200, 5)]:
            shell.chord("Control_L", "0")
            shell.place("Nuschl", 40, 40, width, height)
            for _ in range(steps):
                shell.chord("Control_L", "equal")
                time.sleep(.1)
            scale = percent / 100
            eventually(lambda: abs(json.loads((profile / "config/settings.json").read_text())["appearance"]["zoom"] - scale) < .001, "native zoom not saved")
            add = find("Add a server")
            eventually(lambda: abs(add.extents()[2] - 48 * scale) <= 2, "native zoom not laid out")
            g = geometry(); frame, buffer = g["frame"], g["buffer"]
            viewport = (frame["x"] - buffer["x"], frame["y"] - buffer["y"], frame["width"], frame["height"])
            controls = [find(name) for name in ("Settings", "Unmute", "Deafen")]
            boxes = [node.extents() for node in controls]
            for node, box in zip(controls + [add], boxes + [add.extents()]):
                assert inside(box, viewport), f"{node.name} outside visible frame: {box} {viewport}"
                assert node.call("Action", "GetActions")[0], node.name + " has no activation"
                x, y = node.centre()
                assert node.call("Component", "Contains", "iiu", str(x), str(y), "1")[0], node.name + " center is not hittable"
            gaps = [boxes[i + 1][0] - boxes[i][0] - boxes[i][2] for i in range(2)]
            assert all(gap >= 2 * scale for gap in gaps), f"footer gaps missing at {percent}%: {boxes} {gaps}"
            assert max(box[1] + box[3] / 2 for box in boxes) - min(box[1] + box[3] / 2 for box in boxes) <= 2, "control cluster rows diverged"
            add_box = add.extents(); gutter = viewport[1] + viewport[3] - add_box[1] - add_box[3]
            assert gutter >= 8 * scale, f"Add touches bottom at {label}/{percent}: {gutter}"
            listing = eventually(lambda: named("list", "Servers"), "rail scroll list absent")
            endpoints = []
            for key, node in [("Home", first), ("End", last)]:
                listing.call("Component", "GrabFocus"); shell.key(key)
                def revealed():
                    x, y, w, h = node.extents()
                    return inside((x - 8 * scale, y - 8 * scale, w + 16 * scale, h + 16 * scale), listing.extents())
                eventually(revealed, "first/last rail decoration is clipped")
                endpoints.append({"key": key, "button": node.extents(), "list": listing.extents()})
                assert add.extents() == add_box, "rail scroll moved Add control"
            assert own()["self_muted"] and not own()["self_deafened"], "geometry changed voice consent"
            records.append({"case": label, "requested": [width, height], "zoom": percent, "geometry": g, "viewport": viewport, "add": add_box, "bottom_gutter": gutter, "controls": dict(zip(("Settings", "Unmute", "Deafen"), boxes)), "gaps": gaps, "endpoints": endpoints})
            save(); shot(f"spacing-{label}-{percent}")
            # Real pointer paths, not only accessibility bounds, at both ends of zoom.
            if label == "default" and percent == 100 or label == "narrow-small" and percent == 200:
                pointer(find("Settings")); eventually(lambda: named("dialog", "Settings"), "Settings pointer activation failed")
                shell.key("Escape"); eventually(lambda: named("dialog", "Settings") is None, "Settings did not close")
                pointer(find("Add a server")); eventually(lambda: named("dialog", "Add a server"), "Add pointer activation failed")
                shell.key("Escape"); eventually(lambda: named("dialog", "Add a server") is None, "Add did not close")
                records[-1]["pointer_settings_add"] = True; save()
    shell.chord("Control_L", "0"); shell.place("Nuschl", 40, 40, 1280, 800)
    eventually(lambda: abs(find("Add a server").extents()[2] - 48) <= 2, "tail zoom not ready")
    # Capture is explicitly allowed ONLY against the harness's synthetic null-Pulse mic.
    pointer(find("Unmute")); eventually(lambda: not own()["self_muted"], "Unmute pointer did not reach owned server")
    pointer(find("Mute")); eventually(lambda: own()["self_muted"], "Mute pointer did not restore consent")
    pointer(find("Deafen")); eventually(lambda: own()["self_deafened"] and own()["self_muted"], "Deafen pointer was not acknowledged")
    pointer(find("Undeafen")); eventually(lambda: not own()["self_deafened"] and own()["self_muted"], "Undeafen changed mute consent")
    records.append({"pointer_audio_controls": True, "private_server_acknowledged": True, "final_muted": True}); save()
    print("Native spacing passed:16 actual zoom/window cases, rail endpoints and pointer controls.", flush=True)
