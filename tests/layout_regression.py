#!/usr/bin/env python3
"""Assertions for screenshots.py's real private-GNOME `layout` scene.

Uses actual migration and moderation notices, never injected UI state. Long
unbroken paths and Unicode destinations must not displace fixed controls.
"""
import json
import time
from accessibility import items, snapshot
from local_murmur import eventually
import frame_timing

SERVER_NAME = "Community-" + "verylongunbrokensubdomain" * 8 + "-日本語-👩🏽‍💻"
CHANNEL_NAME = "Development-" + "longunbrokendestination" * 7 + "-Ελληνικά-日本語"

def inside(inner, outer):
    x, y, w, h = inner
    ox, oy, ow, oh = outer
    return w > 0 and h > 0 and x >= ox - 1 and y >= oy - 1 and x + w <= ox + ow + 1 and y + h <= oy + oh + 1

def run(args, root, shell, registry, bots, profile, port, pin, shot):
    assert registry, "layout scene requires a private AT-SPI registry"
    application = eventually(lambda: next((node for node in registry.children() if node.name.lower() == "nuschl"), None), "desktop missing from AT-SPI")
    def named(role, name, prefix=False):
        return next((node for node,r,n,_ in items(application) if (not role or r == role) and (n.startswith(name) if prefix else n == name)),None)
    records=[]
    report=args.output / "layout-proof.json"
    # The archive warning is produced by real profile migration at startup.
    notice=eventually(lambda:named("status bar","Connection notice"),"migration notice absent")
    original=snapshot(notice)
    full="\n".join(name for _,_,_,name in original)
    assert "The experimental Nuschel end-to-end identity was archived at" in full,full
    assert str(profile) in full,"full unbroken absolute path was lost"
    assert (profile / "data/keys.nuschel-v1/layout-evidence").read_text()=="synthetic, not a key\n"
    for label,width,sidebar in [("wide",1280,240),("narrow",900,180),("expanded-sidebar",1280,360)]:
        for percent,steps in [(100,0),(125,2),(150,3),(200,5)]:
            shell.chord("Control_L","0")
            shell.place("Nuschl",40,40,width,800)
            # Actual splitter keyboard API, not a direct config mutation.
            splitter=eventually(lambda:named("role 51","Resize channel list") or named(None,"Resize channel list"),"splitter missing")
            sx,sy=splitter.centre()
            current_window=shell.window("Nuschl")
            shell.drag((current_window["x"]+sx,current_window["y"]+sy),
                       (current_window["x"]+72+sidebar+2,current_window["y"]+sy))
            for _ in range(steps):shell.chord("Control_L","equal");time.sleep(.1)
            eventually(lambda:abs(json.loads((profile/"config/settings.json").read_text())["appearance"]["zoom"]-percent/100)<.001,"real zoom missing")
            time.sleep(.35)
            window=shell.window("Nuschl"); viewport=(0,0,window["width"],window["height"])
            current=eventually(lambda:named("status bar","Connection notice"),"notice disappeared")
            dismiss=eventually(lambda:named("push button","Dismiss notice"),"Dismiss missing")
            disconnect=eventually(lambda:named("push button","Disconnect"),"voice Disconnect missing")
            status=eventually(lambda:named(None,"Voice connected"),"fixed voice connection indicator missing")
            # Native accessibility values retain full paths and route labels.
            names=[name for _,_,_,name in snapshot(current)]
            assert any(str(profile) in name for name in names),"migration text was truncated in accessibility"
            assert any(CHANNEL_NAME in name and SERVER_NAME in name for _,_,name,_ in items(application)),"full voice destination inaccessible"
            entry={"case":label,"width":width,"zoom":percent,"notice":current.extents(),"dismiss":dismiss.extents(),"disconnect":disconnect.extents(),"voice_status":status.extents(),"window":viewport}
            records.append(entry);report.write_text(json.dumps(records,indent=2))
            for key in ["notice","dismiss","disconnect","voice_status"]:assert inside(entry[key],viewport),f"{key} outside viewport: {entry}"
            assert inside(entry["dismiss"],entry["notice"]),f"Dismiss outside notice: {entry}"
            shot(f"layout-{label}-{percent}")
    # Pointer and keyboard both activate the same fixed dismiss control.
    shell.chord("Control_L","0")
    shell.place("Nuschl",40,40,1280,800)
    dismiss=eventually(lambda:named("push button","Dismiss notice"),"Dismiss missing")
    dismiss.call("Component","GrabFocus");shell.key("Return")
    eventually(lambda:named("push button","Dismiss notice") is None,"keyboard dismissal failed")
    records.append({"keyboard_dismissed":True})
    # Reuse compositor timing sweeps, without custom production measurement.
    window=shell.window("Nuschl");w,h=window["width"],window["height"]
    sizes=[[round(w-(w-900)*(1-abs(i-30)/30)),round(h-(h-620)*(1-abs(i-30)/30))] for i in range(61)]
    ticks=shell.resize_sweep("Nuschl",sizes)
    records.append({"resize_ticks":frame_timing.lag(ticks,sizes),"resize_span":frame_timing.moving(ticks,sizes)})
    log=args.output/"frames.log"
    if log.exists():records[-1]["frames"]=frame_timing.summarize(frame_timing.read_log(log),*records[-1]["resize_span"])
    report.write_text(json.dumps(records,indent=2))
    print("Layout: long actual migration path, Unicode voice destinations, viewport bounds, 12 resize/zoom cases and keyboard dismissal passed.",flush=True)
