#!/usr/bin/env python3
"""Assertions for screenshots.py's real private-GNOME `layout` scene.

Uses actual migration and moderation notices, never injected UI state. Long
unbroken paths and Unicode destinations must not displace fixed controls.
"""
import json
import os
import time
from accessibility import items, snapshot
from local_murmur import Murmur, eventually
from mumble_client import Client
import frame_timing

SERVER_NAME = "Community-" + "verylongunbrokentoken" * 4 + "-日本語-👩🏽‍💻"
CHANNEL_NAME = "Development-" + "longdestination" * 5 + "-Ελληνικά-日本語"

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
    suggestion=eventually(lambda:named("push button","Use push-to-talk"),"real server SuggestConfig notice absent")
    full="\n".join(name for _,_,_,name in original)
    assert "The experimental Nuschel end-to-end identity was archived at" in full,full
    assert str(profile) in full,"full unbroken absolute path was lost"
    assert (profile / "data/keys.nuschel-v1/layout-evidence").read_text()=="synthetic, not a key\n"
    cases = [] if "layout-controls" in args.scenes.split(",") else [("wide",1280,240),("narrow",900,200),("expanded-sidebar",1280,360)]
    for label,width,sidebar in cases:
        for percent,steps in [(100,0),(125,2),(150,3),(200,5)]:
            shell.chord("Control_L","0")
            shell.place("Nuschl",40,40,width,800)
            # Actual splitter keyboard API, not a direct config mutation.
            splitter=eventually(lambda:named("role 51","Resize channel list") or named(None,"Resize channel list"),"splitter missing")
            sx,sy=splitter.centre()
            geometry=json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const f=w.get_frame_rect(),b=w.get_buffer_rect();return {frame:{x:f.x,y:f.y,width:f.width,height:f.height},buffer:{x:b.x,y:b.y,width:b.width,height:b.height}};})())"))
            buffer=geometry["buffer"];frame=geometry["frame"]
            shell.drag((buffer["x"]+sx,buffer["y"]+sy),
                       (frame["x"]+72+sidebar+2,buffer["y"]+sy))
            for _ in range(steps):shell.chord("Control_L","equal");time.sleep(.1)
            eventually(lambda:abs(json.loads((profile/"config/settings.json").read_text())["appearance"]["zoom"]-percent/100)<.001,"real zoom missing")
            time.sleep(.35)
            window=shell.window("Nuschl")
            geometry=json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const f=w.get_frame_rect(),b=w.get_buffer_rect();return {frame:{x:f.x,y:f.y,width:f.width,height:f.height},buffer:{x:b.x,y:b.y,width:b.width,height:b.height}};})())"))
            frame,buffer=geometry["frame"],geometry["buffer"]
            # AT-SPI window coordinates include the compositor's invisible CSD
            # buffer inset. Derive that inset from actual rectangles, not a
            # hardcoded padding or a relaxation of the visible frame bounds.
            viewport=(frame["x"]-buffer["x"],frame["y"]-buffer["y"],frame["width"],frame["height"])
            current=eventually(lambda:named("status bar","Connection notice"),"notice disappeared")
            dismiss=eventually(lambda:named("push button","Dismiss notice"),"Dismiss missing")
            disconnect=eventually(lambda:named("push button",f"Disconnect from {SERVER_NAME} (voice and chat)"),"voice Disconnect missing")
            status=eventually(lambda:named(None,"Voice connected"),"fixed voice connection indicator missing")
            stack=eventually(lambda:named(None,"Notifications"),"bounded notification stack missing")
            dismiss.call("Component","GrabFocus")
            eventually(lambda:inside(dismiss.extents(),stack.extents()),"initial dismiss focus was not revealed")
            # Native accessibility values retain full paths and route labels.
            names=[name for _,_,_,name in snapshot(current)]
            assert any(str(profile) in name for name in names),"migration text was truncated in accessibility"
            assert any(CHANNEL_NAME in name and SERVER_NAME in name for _,_,name,_ in items(application)),"full voice destination inaccessible"
            entry={"geometry":geometry,"stack":stack.extents(),"case":label,"width":width,"zoom":percent,"notice":current.extents(),"dismiss":dismiss.extents(),"disconnect":disconnect.extents(),"voice_status":status.extents(),"window":viewport}
            records.append(entry);report.write_text(json.dumps(records,indent=2))
            shot(f"layout-{label}-{percent}")
            for key in ["stack","dismiss","disconnect","voice_status"]:assert inside(entry[key],viewport),f"{key} outside viewport: {entry}"
            assert entry["stack"][3] <= viewport[3] * .35, f"notice stack exceeded its shared viewport budget: {entry}"
            assert inside(entry["dismiss"],entry["stack"]),f"Dismiss outside visible stack: {entry}"
            assert inside(entry["dismiss"],entry["notice"]),f"Dismiss outside notice: {entry}"
            if entry["notice"][3] > entry["stack"][3] + 10:
                initial_y=current.extents()[1]
                stack.call("Component","GrabFocus");shell.key("End")
                eventually(lambda:current.extents()[1] < initial_y-5,"End did not scroll the focused notice stack")
                shell.key("Home")
                eventually(lambda:inside(dismiss.extents(),stack.extents()),"Home did not restore visible dismiss")
                entry["keyboard_scrolled"] = True
                shell.key("End")
                eventually(lambda:not inside(dismiss.extents(),stack.extents()),"End did not move dismiss outside the clip before focus probe")
                dismiss.call("Component","GrabFocus")
                eventually(lambda:inside(dismiss.extents(),stack.extents()),"focusing a clipped Dismiss did not reveal it")
                entry["focused_action_revealed"] = True
                if label == "wide" and percent == 125:
                    stack.call("Component", "GrabFocus");shell.key("End")
                    eventually(lambda:not inside(dismiss.extents(),stack.extents()),"Tab probe did not begin with clipped action")
                    shell.key("Tab")
                    eventually(lambda:dismiss.focused,"Tab skipped the first notice action")
                    eventually(lambda:inside(dismiss.extents(),stack.extents()),"Tab focus did not reveal the clipped action")
                    entry["tab_revealed"] = True
            suggestion.call("Component","GrabFocus")
            eventually(lambda:inside(suggestion.extents(),stack.extents()),"secondary notice action was not revealed")
            entry["secondary_action"] = suggestion.extents()
            assert inside(entry["secondary_action"],viewport),f"secondary notice button escaped: {entry}"
            dismiss.call("Component","GrabFocus")
            eventually(lambda:inside(dismiss.extents(),stack.extents()),"returning to first notice did not reveal Dismiss")
            # The search action is mandatory even when the header wraps.
            search=eventually(lambda:named("push button","Search messages"),"header search missing")
            assert inside(search.extents(),viewport),f"header toolbar escaped: {entry}"
            report.write_text(json.dumps(records,indent=2))
    # Pointer and keyboard both activate the same fixed dismiss control.
    shell.chord("Control_L","0")
    shell.place("Nuschl",40,40,1280,800)
    dismiss=eventually(lambda:named("push button","Dismiss notice"),"Dismiss missing")
    dismiss.call("Component","GrabFocus");shell.key("Return")
    eventually(lambda:named("push button","Dismiss notice") is None,"keyboard dismissal failed")
    records.append({"keyboard_dismissed":True});report.write_text(json.dumps(records,indent=2))
    # A separate actual server notice exercises pointer hit testing, after
    # the first warning was genuinely dismissed with the keyboard.
    password=root/"layout-moderator-password"
    password.write_text(Murmur.SUPERUSER_PASSWORD);password.chmod(0o600)
    moderator=Client(args.binaries,"127.0.0.1",port,pin,"SuperUser",root/"layout-moderator",["--password-file",str(password)])
    bots.append(moderator)
    session=eventually(lambda:next((user["session"] for user in bots[0].users.values() if user["name"]=="Andi"),None),"Andi absent before pointer notice")
    reason="layout-pointer-dismiss: "+"/private/unbroken-token"*30
    moderator.send(kind="moderate",session=session,action="kick",reason=reason)
    current=eventually(lambda:named("status bar","Connection notice"),"moderation notice absent")
    eventually(lambda:any(reason in name for _,_,_,name in snapshot(current)),"full moderation reason inaccessible")
    dismiss=eventually(lambda:named("push button","Dismiss notice"),"pointer Dismiss missing")
    dismiss.call("Component","GrabFocus")
    time.sleep(.2)
    geometry=json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"))
    x,y=dismiss.centre();shell.click(geometry["x"]+x,geometry["y"]+y)
    eventually(lambda:named("push button","Dismiss notice") is None,"actual pointer dismissal failed")
    records.append({"pointer_dismissed":True,"reason":reason});report.write_text(json.dumps(records,indent=2))
    # Shared compact controls and modal scroll/reveal remain usable at both
    # ordinary and narrow/high-zoom sizes; no microphone test is activated.
    for width,percent,steps in [(1280,100,0),(900,200,5)]:
        shell.chord("Control_L","0");shell.place("Nuschl",40,40,width,800)
        for _ in range(steps):shell.chord("Control_L","equal");time.sleep(.1)
        settings=eventually(lambda:named("push button","Settings"),"Settings action absent")
        settings.press()
        dialog=eventually(lambda:named("dialog","Settings"),"Settings dialog absent")
        done=eventually(lambda:named("push button","Done"),"compact Done absent")
        done.call("Component","GrabFocus")
        eventually(lambda:inside(done.extents(),dialog.extents()),"Done escaped Settings")
        assert done.extents()[2] < 130*percent/100,"short button expanded into a full-width action"
        # Shared radio/switch/slider families reveal without activating audio.
        for label in ["Voice activity", "Voice activity sensitivity", "Show when you are typing"]:
            control=eventually(lambda:named(None,label),f"Settings control {label} absent")
            dialog.call("Component","GrabFocus");shell.key("Home")
            control.call("Component","GrabFocus")
            eventually(lambda:inside(control.extents(),dialog.extents()),f"Settings control {label} was not revealed")
            records.append({"settings_control":label,"width":width,"zoom":percent,"bounds":control.extents(),"dialog":dialog.extents()})
        # The font list has its own handle; its focused descendant must reveal
        # through BOTH the inner list and the enclosing modal, even after Home.
        opener=eventually(lambda:named("push button","Interface font:",True),"font picker opener absent")
        opener.call("Component","GrabFocus")
        eventually(lambda:inside(opener.extents(),dialog.extents()),"font picker opener was not revealed")
        fonts=named(None,"Installed fonts")
        if not fonts:
            opener.press()
            fonts=eventually(lambda:named(None,"Installed fonts"),"font choices absent")
        options=[node for _,node,role,_ in snapshot(fonts) if role=="radio button"]
        assert len(options)>1,"fixture must expose a real nested font list"
        choice=options[-1]
        for attempt in range(2):
            dialog.call("Component","GrabFocus");shell.key("Home")
            eventually(lambda:not inside(choice.extents(),dialog.extents()),"nested font probe did not begin clipped")
            choice.call("Component","GrabFocus")
            eventually(lambda:inside(choice.extents(),fonts.extents()) and inside(choice.extents(),dialog.extents()),"nested font choice was not revealed through both regions")
            records.append({"nested_font_focus":choice.name,"attempt":attempt,"width":width,"zoom":percent,"bounds":choice.extents(),"list":fonts.extents(),"dialog":dialog.extents()})
        report.write_text(json.dumps(records,indent=2))
        controls=[node for _,node,role,_ in snapshot(dialog) if role=="push button"]
        last=controls[-1]
        last.call("Component","GrabFocus")
        eventually(lambda:inside(last.extents(),dialog.extents()),f"last Settings action {last.name} was not revealed")
        records.append({"modal":"Settings","width":width,"zoom":percent,"last_control":last.name,"bounds":last.extents(),"dialog":dialog.extents()})
        shell.key("Escape")
        eventually(lambda:named("dialog","Settings") is None,"Settings Escape did not close")
        add=eventually(lambda:named("push button","Add a server"),"Add server absent")
        add.press()
        dialog=eventually(lambda:named("dialog","Add or connect to a Mumble server"),"server form absent")
        cancel=eventually(lambda:named("push button","Cancel"),"server Cancel absent")
        cancel.call("Component","GrabFocus")
        records.append({"modal":"Server diagnostic","width":width,"zoom":percent,"cancel":cancel.extents(),"dialog":dialog.extents(),"focused":cancel.focused})
        report.write_text(json.dumps(records,indent=2));shot(f"server-form-{width}-{percent}")
        try:
            eventually(lambda:inside(cancel.extents(),dialog.extents()),"server form Cancel was not revealed",timeout=5)
        except AssertionError:
            records.append({"modal":"Server post-focus failure","cancel":cancel.extents(),"dialog":dialog.extents(),"focused":cancel.focused})
            report.write_text(json.dumps(records,indent=2));shot(f"server-post-focus-{width}-{percent}")
            dialog.call("Component","GrabFocus");shell.key("End");time.sleep(.7)
            records.append({"modal":"Server manual End diagnostic","cancel":cancel.extents(),"dialog":dialog.extents(),"focused":cancel.focused})
            shell.key("Home");time.sleep(.3);cancel.call("Component","GrabFocus");time.sleep(.7)
            records.append({"modal":"Server re-focus diagnostic","cancel":cancel.extents(),"dialog":dialog.extents(),"focused":cancel.focused})
            report.write_text(json.dumps(records,indent=2));shot(f"server-refocus-{width}-{percent}")
            raise
        records.append({"modal":"Server form","width":width,"zoom":percent,"cancel":cancel.extents(),"dialog":dialog.extents()})
        cancel.press()
        eventually(lambda:named("dialog","Add or connect to a Mumble server") is None,"Cancel did not close server form")
        rail=eventually(lambda:named("push button",SERVER_NAME,True),"server rail action absent")
        geometry=json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"))
        x,y=rail.centre();shell.click(geometry["x"]+x,geometry["y"]+y,button=3)
        edit=eventually(lambda:named("menu item","Edit server…"),"server context edit absent")
        edit.press()
        dialog=eventually(lambda:named("dialog","Add or connect to a Mumble server"),"edit form absent")
        save_label="Save" if named("push button","Save") else "Save and connect"
        for label in ["Remove saved server","Cancel",save_label]:
            action=eventually(lambda:named("push button",label),f"edit form action {label} absent")
            action.call("Component","GrabFocus")
            eventually(lambda:inside(action.extents(),dialog.extents()),f"edit form action {label} escaped")
            records.append({"edit_action":label,"width":width,"zoom":percent,"bounds":action.extents(),"dialog":dialog.extents()})
        shot(f"edit-form-{width}-{percent}")
        named("push button","Cancel").press()
        eventually(lambda:named("dialog","Add or connect to a Mumble server") is None,"edit Cancel did not close")
    shell.chord("Control_L","0");shell.place("Nuschl",40,40,1280,800)
    # Reuse compositor timing sweeps, without custom production measurement.
    window=shell.window("Nuschl");w,h=window["width"],window["height"]
    sizes=[[round(w-(w-900)*(1-abs(i-30)/30)),round(h-(h-620)*(1-abs(i-30)/30))] for i in range(61)]
    ticks=shell.resize_sweep("Nuschl",sizes)
    records.append({"resize_ticks":frame_timing.lag(ticks,sizes),"resize_span":frame_timing.moving(ticks,sizes)})
    log=args.output/"frames.log"
    if log.exists():records[-1]["frames"]=frame_timing.summarize(frame_timing.read_log(log),*records[-1]["resize_span"])
    report.write_text(json.dumps(records,indent=2))
    print(f"Layout: {len(cases)*4} width/zoom cases; keyboard/pointer dismissal and Settings/server-form focus checks passed.",flush=True)


def performance(args, shell, profile, shot):
    """Same real long-notice fixture before/after, with frame-timing builds."""
    log = args.output / "frames.log"
    eventually(log.exists, "frame-timing executable required; no timings were logged")
    measured = {"host": {"load_average": os.getloadavg(), "cpus": os.cpu_count(), "limits": "No other owned build/test jobs during benchmark; unowned host workloads remain running."}}
    for percent, steps in [(100, 0), (125, 2)]:
        shell.chord("Control_L", "0")
        shell.place("Nuschl", 40, 40, 1280, 800)
        for _ in range(steps):
            shell.chord("Control_L", "equal")
            time.sleep(.1)
        eventually(lambda: abs(json.loads((profile / "config/settings.json").read_text())["appearance"]["zoom"] - percent / 100) < .001, "actual benchmark zoom missing")
        time.sleep(1)
        shot(f"performance-{percent}")
        begin = int(shell.eval("imports.gi.GLib.get_monotonic_time()"))
        time.sleep(3)
        end = int(shell.eval("imports.gi.GLib.get_monotonic_time()"))
        measured[f"idle-{percent}"] = {"span": [begin, end], "frames": frame_timing.summarize(frame_timing.read_log(log), begin, end)}
        sizes = [[round(1280 - 380 * (1 - abs(i - 60) / 60)), round(800 - 180 * (1 - abs(i - 60) / 60))] for i in range(121)]
        ticks = shell.resize_sweep("Nuschl", sizes)
        span = frame_timing.moving(ticks, sizes)
        measured[f"sweep-{percent}"] = {"span": span, "ticks": frame_timing.lag(ticks, sizes), "frames": frame_timing.summarize(frame_timing.read_log(log), *span)}
        time.sleep(.5)
        shell.place("Nuschl", 40, 40, 1280, 800)
        time.sleep(.8)
        window = shell.window("Nuschl")
        corner = (window["x"] + window["width"] + 4, window["y"] + window["height"] + 4)
        path, sizes = [], []
        for i in range(126):
            dx, dy = round(-380 * i / 125), round(-180 * i / 125)
            path.append([corner[0] + dx, corner[1] + dy])
            sizes.append([1280 + dx, 800 + dy])
        ticks = shell.drag_resize("Nuschl", corner, path)
        span = frame_timing.moving(ticks, sizes)
        measured[f"drag-{percent}"] = {"span": span, "ticks": frame_timing.lag(ticks, sizes), "frames": frame_timing.summarize(frame_timing.read_log(log), *span)}
        (args.output / "performance.json").write_text(json.dumps(measured, indent=2))
    print("Matched frame-timing fixture captured actual idle/sweep/drag at 100% and 125%.", flush=True)
