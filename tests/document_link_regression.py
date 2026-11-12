"""Native document-link actions; all HTTPS launches end in an owned handler."""

import json
import sqlite3
import sys
import time

from accessibility import items
from local_murmur import eventually

WELCOME_EXTRA = (
    "<br/><a href='https://welcome.test/keyboard'>Welcome keyboard</a> "
    "<a href='https://welcome.test/assistive'>Welcome assistive</a><br/>"
    "<a href='https://welcome.test/wrapped'>Wrapped 日本語 link "
    + "across several words and rows "
    * 10
    + "end</a>"
    "<br/><a href='javascript:alert(1)'>Refused unsafe target</a>"
)
DESCRIPTION = (
    "<a href='https://d.test/p'>Desc P</a> "
    "<a href='https://d.test/k'>Desc K</a> "
    "<a href='https://d.test/a'>Desc A</a>"
)
COMMENT = "Comment https://c.test/p"


def seed_description(database):
    with sqlite3.connect(database) as db:
        db.execute(
            "INSERT OR REPLACE INTO channel_info (server_id, channel_id, key, value) VALUES (1, 0, 0, ?)",
            (DESCRIPTION,),
        )


def configure_handler(shell, root, output):
    log = output / "url-handler.jsonl"
    handler = root / "url-handler"
    handler.write_text(
        f"#!{sys.executable}\nimport json,sys\nwith open({str(log)!r}, 'a') as stream: stream.write(json.dumps(sys.argv[1:])+'\\n')\n"
    )
    handler.chmod(0o700)
    applications = shell.home / "data/applications"
    applications.mkdir(parents=True, exist_ok=True)
    desktop = "nuschl-private-document-link.desktop"
    (applications / desktop).write_text(
        f"[Desktop Entry]\nType=Application\nName=Nuschl private link fixture\nExec={handler} %u\nNoDisplay=true\nTerminal=false\nStartupNotify=false\nMimeType=x-scheme-handler/https;\n"
    )
    association = "[Default Applications]\nx-scheme-handler/https=" + desktop + "\n"
    (shell.home / "config").mkdir(parents=True, exist_ok=True)
    (shell.home / "config/mimeapps.list").write_text(association)
    (applications / "mimeapps.list").write_text(association)
    return {
        "BROWSER": str(handler),
        "RUST_LOG": "nuschl=info,nuschl::document_links=debug",
        "NUSCHL_FRAME_TIMING": str(output / "frames.log"),
    }


def run_actions(args, root, shell, application, bot, murmur):
    # The headless compositor has no keyboard seat until this private virtual
    # input device exists. Create it before asking AT-SPI to focus a node.
    shell.key("Shift_L")
    shell.place("Nuschl", 40, 40, 1280, 800)
    log = args.output / "url-handler.jsonl"
    records = []
    desktop_log = args.output / "desktop.log"
    initial_geometry = desktop_log.read_text().count("document link geometry updated")

    def launches():
        return (
            [json.loads(line)[0] for line in log.read_text().splitlines()]
            if log.exists()
            else []
        )

    def find(name, role=None):
        return next(
            (
                node
                for node, kind, label, _ in items(application)
                if label == name and (role is None or kind == role)
            ),
            None,
        )

    def link(name):
        return eventually(
            lambda: find(name, "role 88") or find(name, "link"), f"link missing: {name}"
        )

    def position(node, first=False):
        geometry = json.loads(
            shell.eval(
                "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"
            )
        )
        x, y, width, height = node.extents()
        return geometry["x"] + x + (5 if first else width // 2), geometry["y"] + y + (
            8 if first else height // 2
        )

    def expected(url, action):
        before = len(launches())
        action()
        eventually(
            lambda: len(launches()) > before,
            f"isolated URL handler did not receive {url}",
        )
        assert launches()[before:] == [url], launches()
        records.append(url)

    def keyboard(name, url):
        node = link(name)
        assert node.call("Component", "GrabFocus")[0], f"focus action refused: {name}"
        eventually(lambda: node.focused, f"keyboard focus missing: {name}")
        expected(url, lambda: shell.key("Return"))

    def pointer(name, url):
        node = link(name)
        assert node.call("Component", "GrabFocus")[0], (
            f"pointer focus action refused: {name}"
        )
        eventually(lambda: node.focused, f"pointer target focus missing: {name}")
        time.sleep(0.2)
        expected(url, lambda: shell.click(*position(node)))

    pointer("Welcome help", "https://example.test/welcome")
    keyboard("Welcome keyboard", "https://welcome.test/keyboard")
    shell.key("Tab")
    eventually(
        lambda: link("Welcome assistive").focused,
        "Tab did not move to the next document link",
    )
    expected(
        "https://welcome.test/assistive", lambda: link("Welcome assistive").press()
    )
    assert find("Refused unsafe target", "role 88") is None
    assert find("Refused unsafe target", "link") is None

    wrapped = eventually(
        lambda: next(
            (
                node
                for node, role, name, _ in items(application)
                if role in ("role 88", "link") and name.startswith("Wrapped 日本語")
            ),
            None,
        ),
        "wrapped Unicode link missing",
    )
    wrapped.call("Component", "GrabFocus")
    eventually(lambda: wrapped.focused, "wrapped link focus missing")
    expected(
        "https://welcome.test/wrapped",
        lambda: shell.click(*position(wrapped, first=True)),
    )
    before = launches()
    point = position(wrapped, first=True)
    shell.drag(point, (point[0] + 35, point[1]))
    time.sleep(0.3)
    assert launches() == before, "a drag activated a link"

    assert (
        desktop_log.read_text().count("document link geometry updated")
        == initial_geometry
    ), "focus-only navigation recomputed document geometry"
    # Changing width reuses the text shaper's new line layout, including focus
    # rectangles for a link spanning several visual rows.
    shell.place("Nuschl", 40, 40, 900, 800)
    wrapped.call("Component", "GrabFocus")
    expected("https://welcome.test/wrapped", lambda: shell.key("Return"))
    shell.screenshot(args.output / "document-links-wrapped.png")
    shell.place("Nuschl", 40, 40, 1360, 800)
    for _ in range(5):
        shell.chord("Control_L", "equal")
    time.sleep(0.4)
    eventually(lambda: wrapped.focused, "zoom lost retained link focus")
    expected("https://welcome.test/wrapped", lambda: shell.key("Return"))
    keyboard("Welcome help", "https://example.test/welcome")
    pointer("Welcome help", "https://example.test/welcome")
    shell.screenshot(args.output / "document-links-200.png")
    shell.chord("Control_L", "0")
    shell.place("Nuschl", 40, 40, 900, 800)
    time.sleep(0.3)
    pointer("Desc P", "https://d.test/p")
    keyboard("Desc K", "https://d.test/k")
    expected(
        "https://d.test/a",
        lambda: link("Desc A").press(),
    )

    def open_profile():
        user = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "tree item" and name.startswith("Control sender")
                ),
                None,
            ),
            "comment user missing",
        )
        shell.click(*position(user), button=3)
        eventually(
            lambda: find("Profile and connection"), "profile menu entry missing"
        ).press()
        return link("https://c.test/p")

    background = link("Welcome help")
    comment = open_profile()
    before = launches()
    background.press()
    time.sleep(0.2)
    assert launches() == before, "a background document activated behind a modal"
    pointer("https://c.test/p", "https://c.test/p")
    keyboard("https://c.test/p", "https://c.test/p")
    expected(
        "https://c.test/p",
        lambda: link("https://c.test/p").press(),
    )
    shell.screenshot(args.output / "document-links-comment.png")

    # The old UI cache can remain visible until the profile is reopened. Its
    # current-model/source guard must refuse the old link immediately.
    bot.send(kind="comment", text="Replacement https://comment.test/new")
    bot.send(kind="blobs", comments=[bot.session])
    bot.wait(
        lambda event: (
            event["kind"] == "user"
            and event["user"]["session"] == bot.session
            and "new" in (event["user"].get("comment") or "")
        )
    )
    time.sleep(0.3)
    before = launches()
    comment.press()
    time.sleep(0.3)
    assert launches() == before, "stale comment document activated"
    eventually(lambda: find("Close"), "profile close missing").press()
    open_profile_new = lambda: next(
        (
            node
            for node, role, name, _ in items(application)
            if role == "tree item" and name.startswith("Control sender")
        ),
        None,
    )
    shell.click(
        *position(eventually(open_profile_new, "profile user missing")), button=3
    )
    eventually(lambda: find("Profile and connection"), "profile menu missing").press()
    expected(
        "https://comment.test/new", lambda: link("https://comment.test/new").press()
    )
    eventually(lambda: find("Close"), "profile close missing").press()

    # Actual disconnect/reconnect creates a fresh native connection generation
    # and a fresh document lease at the same UI location.
    old = link("Welcome help")
    eventually(lambda: find("Disconnect", "push button"), "Disconnect missing").press()
    eventually(lambda: find("Connect", "push button"), "disconnected state missing")
    before = launches()
    try:
        old.press()
    except RuntimeError:
        pass
    assert launches() == before, "disconnected document activated"
    bot.close()
    murmur.stop()
    murmur.ini.write_text(
        murmur.ini.read_text().replace(
            "https://example.test/welcome", "https://example.test/reconnected"
        )
    )
    murmur.start()
    find("Connect", "push button").press()
    expected("https://example.test/reconnected", lambda: link("Welcome help").press())

    # The geometry cache and frame source must settle at rest. Pointer motion
    # within one target and typing elsewhere must not repeatedly reshape links.
    before = launches()
    composer = eventually(
        lambda: next(
            (
                node
                for node, role, name, _ in items(application)
                if role in ("text", "entry") and name == "Message"
            ),
            None,
        ),
        "composer missing",
    )
    composer.call("Component", "GrabFocus")
    shell.type("typing only")
    time.sleep(0.5)
    assert launches() == before
    # A focused textarea legitimately blinks its caret. Move to a document
    # link without activating it before measuring an otherwise idle window.
    link("Welcome help").call("Component", "GrabFocus")
    eventually(lambda: link("Welcome help").focused, "idle focus missing")
    time.sleep(3)
    desktop_log = args.output / "desktop.log"
    count = desktop_log.read_text().count("document link geometry updated")
    frames = args.output / "frames.log"
    frame_count = len(frames.read_text().splitlines()) if frames.exists() else None
    # A connected client still receives meaningful transport/latency updates.
    # Require a continuous quiet interval, recording intervening notifications;
    # an animation loop cannot pass by picking one lucky instantaneous sample.
    idle_started = quiet_since = time.monotonic()
    idle_start_us = time.monotonic_ns() // 1000
    notification_lines = 0
    longest_quiet = 0.0
    while time.monotonic() - idle_started < 8:
        time.sleep(0.1)
        now = time.monotonic()
        if frame_count is not None:
            current = len(frames.read_text().splitlines())
            if current != frame_count:
                longest_quiet = max(longest_quiet, now - quiet_since)
                notification_lines += current - frame_count
                frame_count = current
                quiet_since = now
        longest_quiet = max(longest_quiet, now - quiet_since)
    idle = {
        "start_us": idle_start_us,
        "end_us": time.monotonic_ns() // 1000,
        "observed_seconds": round(time.monotonic() - idle_started, 3),
        "longest_quiet_seconds": round(longest_quiet, 3),
        "notification_log_lines": notification_lines,
        "geometry_rebuilds": desktop_log.read_text().count(
            "document link geometry updated"
        )
        - count,
    }
    (args.output / "idle.json").write_text(json.dumps(idle, indent=2) + "\n")
    assert longest_quiet >= 2, "no two-second quiet window: possible idle redraw loop"
    assert idle["geometry_rebuilds"] == 0, "idle link geometry work"
    if frames.exists():
        # The collector is drained by the next Desktop render. Flush the last
        # idle draw after the observation, keeping its original timestamp.
        shell.place("Nuschl", 40, 40, 901, 800)
        time.sleep(0.2)
    report = {
        "activations": records,
        "wrapped_unicode": True,
        "width_and_200_percent_zoom_retained_focus": True,
        "full_inline_documents_only": True,
        "drag_rejected": True,
        "stale_source_rejected": True,
        "inactive_surface_rejected": True,
        "new_generation_target": True,
        "typing_did_not_activate": True,
        "idle_geometry_stable": True,
        "idle_settle_seconds": 3,
        "idle_quiet_seconds": 2,
        "idle": idle,
        "focus_reused_geometry": True,
        "profiled_two_second_quiet_window": frame_count is not None,
    }
    (args.output / "document-links-proof.json").write_text(
        json.dumps(report, indent=2) + "\n"
    )
    return report
