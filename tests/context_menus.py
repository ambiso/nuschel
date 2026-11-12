#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Private native admin-menu reachability at narrow sizes and high zoom."""

import argparse
import json
import re
import sqlite3
import time
from pathlib import Path

import document_link_regression as fixture
import welcome_html
from accessibility import ROLES, Node, items
from layout_regression import inside
from local_murmur import eventually
from mumble_client import Client

TARGET = "Menu target " + "LongUnbrokenName" * 6 + "日本語"


def seed(database):
    # This is a disposable loopback fixture, never an operator's configuration.
    # All real moderation entries must be present, not a shortened synthetic menu.
    with sqlite3.connect(database) as db:
        db.execute(
            "INSERT OR REPLACE INTO acl (server_id, channel_id, priority, group_name, "
            "apply_here, apply_sub, grantpriv, revokepriv) VALUES (1, 0, 99, 'all', 1, 1, ?, 0)",
            (0x1F0FFF,),
        )


def configure(shell, root, output):
    return {
        "RUST_LOG": "nuschl=info,nuschl::layout=debug",
        "NUSCHL_FRAME_TIMING": str(output / "frames.log"),
    }


def actions(args, root, shell, application, bot, murmur):
    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]
    target = Client(
        args.binaries, "127.0.0.1", murmur.port, pin, TARGET, root / "menu-target"
    )
    records = []

    def nodes(parent=application):
        if parent is application:
            return items(application)
        # The common items() helper intentionally returns the entire cache.
        # Filter actual cached parent references when a dialog/menu is scoped.
        data = application.bus(
            "call",
            application.destination,
            "/org/a11y/atspi/cache",
            "org.a11y.atspi.Cache",
            "GetItems",
        )[0]
        parents = {tuple(row[0]): tuple(row[2]) for row in data}
        wanted = (parent.destination, parent.path)
        result = []
        for row in data:
            current = tuple(row[0])
            for _ in range(64):
                if current == wanted:
                    destination, path = row[0]
                    result.append(
                        (
                            Node(application.bus, destination, path),
                            ROLES.get(row[7], f"role {row[7]}"),
                            row[6],
                            row[9][0],
                        )
                    )
                    break
                previous = current
                current = parents.get(current)
                if current is None or current == previous:
                    break
        return result

    def named(name, role=None, parent=application):
        return next(
            (
                node
                for node, kind, label, _ in nodes(parent)
                if label == name and (role is None or kind == role)
            ),
            None,
        )

    def menu():
        return next(
            (node for node, role, _, _ in items(application) if role == "menu"), None
        )

    def geometry():
        return json.loads(
            shell.eval(
                "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y,width:b.width,height:b.height};})())"
            )
        )

    def point(node):
        g = geometry()
        x, y, width, height = node.extents()
        return g["x"] + x + width // 2, g["y"] + y + height // 2

    def close_menu():
        if menu():
            shell.key("Escape")
            eventually(lambda: menu() is None, "Escape did not dismiss menu")

    def target_row():
        return eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "tree item" and name.startswith(TARGET)
                ),
                None,
            ),
            "long-named user missing",
        )

    def open_user():
        row = target_row()
        shell.click(*point(row), button=3)
        try:
            current = eventually(menu, "user menu missing", timeout=5)
        except AssertionError:
            shell.screenshot(args.output / "missing-menu.png")

            def bounds(node):
                try:
                    return node.extents()
                except RuntimeError:
                    return None

            (args.output / "missing-menu-tree.json").write_text(
                json.dumps(
                    [
                        {"role": role, "name": name, "bounds": bounds(node)}
                        for node, role, name, _ in items(application)
                    ],
                    indent=2,
                )
                + "\n"
            )
            shell.screenshot(args.output / "missing-menu.png")
            raise
        eventually(
            lambda: named("Ban…", "menu item", current),
            "full administrative menu missing",
        )
        return current

    def control(name, current):
        return eventually(
            lambda: named(name, "menu item", current), f"menu action absent: {name}"
        )

    def viewport():
        g = geometry()
        return (0, 0, g["width"], g["height"])

    def assert_visible(node, current, label):
        region = named("Context menu actions", parent=current)
        clip = region.extents() if region else current.extents()
        eventually(
            lambda: inside(node.extents(), clip),
            f"{label} is not fully reachable in menu clip",
        )
        assert inside(node.extents(), viewport()), f"{label} escaped window"

    def cancel_prompt():
        dialog = eventually(
            lambda: named("Input", "dialog"), "Ban action did not open reason prompt"
        )
        eventually(
            lambda: named("Cancel", "push button", dialog), "Cancel missing"
        ).press()
        eventually(
            lambda: named("Input", "dialog") is None, "Cancel did not dismiss prompt"
        )
        assert target.process.poll() is None, "fixture target was actually moderated"

    def check_prompt_editing():
        current = open_user()
        action = control("Set nickname…", current)
        action.call("Component", "GrabFocus")
        assert_visible(action, current, "nickname action")
        shell.click(*point(action))
        dialog = eventually(lambda: named("Input", "dialog"), "nickname prompt missing")
        entry = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in nodes(dialog)
                    if role in ("entry", "text") and name.startswith("Nickname for ")
                ),
                None,
            ),
            "nickname input missing",
        )
        shell.click(*point(entry))
        shell.type("sample")
        shell.key("Home")
        shell.type("A")
        shell.key("End")
        shell.type("B")
        shell.key("Left")
        shell.type("X")
        shell.key("Right")
        shell.type("Y")
        eventually(
            lambda: named("Save", "push button", dialog), "nickname Save missing"
        ).press()
        certificate = target.users[target.session]["certificate_hash"]
        settings = root / "profile/config/settings.json"
        time.sleep(0.3)
        actual = json.loads(settings.read_text()).get("nicknames", {}).get(certificate)
        (args.output / "prompt-result.json").write_text(
            json.dumps({"expected": "AsampleXBY", "actual": actual}, indent=2) + "\n"
        )
        assert actual == "AsampleXBY", f"native prompt editing mismatch: {actual!r}"

        return {
            "saved_private_nickname": "AsampleXBY",
            "keys": ["Home", "End", "Left", "Right"],
        }

    def observe_idle():
        frames = args.output / "frames.log"
        if not frames.exists():
            return None
        time.sleep(3)
        desktop_log = args.output / "desktop.log"
        reveals = desktop_log.read_text().count("layout region received reveal")
        count = len(frames.read_text().splitlines())
        started = quiet_since = time.monotonic()
        start_us = time.monotonic_ns() // 1000
        longest = 0.0
        while time.monotonic() - started < 8:
            time.sleep(0.1)
            now = time.monotonic()
            current = len(frames.read_text().splitlines())
            if current != count:
                longest = max(longest, now - quiet_since)
                quiet_since, count = now, current
            longest = max(longest, now - quiet_since)
        result = {
            "start_us": start_us,
            "end_us": time.monotonic_ns() // 1000,
            "observed_seconds": time.monotonic() - started,
            "longest_quiet_seconds": longest,
            "new_focus_reveals": desktop_log.read_text().count(
                "layout region received reveal"
            )
            - reveals,
        }
        assert longest >= 2, "menu never has a continuous two-second quiet interval"
        assert result["new_focus_reveals"] == 0, "idle menu repeated focus reveal work"
        (args.output / "idle.json").write_text(json.dumps(result, indent=2) + "\n")
        return result

    def close_profile():
        dialog = eventually(
            lambda: named("Details", "dialog"), "profile dialog missing"
        )
        eventually(
            lambda: named("Done", "push button", dialog), "profile Done missing"
        ).press()
        eventually(
            lambda: named("Details", "dialog") is None, "profile remained after Done"
        )

    try:
        shell.key("Shift_L")
        if args.only_prompt:
            return {
                "passed": True,
                "prompt_editing": check_prompt_editing(),
                "diagnostic_only": True,
            }
        for width, height, zoom, steps in [
            (1280, 800, 100, 0),
            (900, 600, 100, 0),
            (900, 800, 200, 5),
        ]:
            close_menu()
            shell.chord("Control_L", "0")
            shell.place("Nuschl", 40, 40, width, height)
            for _ in range(steps):
                shell.chord("Control_L", "equal")
            time.sleep(0.3)
            current = open_user()
            entries = [
                name for _, role, name, _ in nodes(current) if role == "menu item"
            ]
            assert len(entries) >= 12, entries
            first = control("Profile and connection", current)
            last = control("Ban…", current)
            record = {
                "width": width,
                "height": height,
                "zoom": zoom,
                "entries": entries,
                "menu": current.extents(),
                "viewport": viewport(),
                "baseline_clipped": not inside(current.extents(), viewport()),
            }
            records.append(record)
            (args.output / "context-menu-cases.json").write_text(
                json.dumps(records, indent=2) + "\n"
            )
            if args.expect_clipped:
                if zoom == 200:
                    assert record["baseline_clipped"], (
                        "old tall menu clipping did not reproduce"
                    )
                    shell.screenshot(args.output / "context-menu-baseline.png")
                    return {
                        "passed": False,
                        "expected_baseline_failure": True,
                        "cases": records,
                    }
                close_menu()
                continue
            assert not record["baseline_clipped"], record
            long = control("Register " + TARGET, current)
            assert long.extents()[2] <= current.extents()[2], "long action widened menu"
            assert long.extents()[3] > first.extents()[3], (
                "long action was truncated instead of wrapped"
            )
            # The menu acquires its own focus scope on open. Tab reaches the
            # first action; reverse traversal wraps to the final admin action.
            shell.key("Tab")
            eventually(
                lambda first=first: first.focused, "Tab did not enter first menu action"
            )
            shell.chord("Shift_L", "Tab")
            eventually(
                lambda last=last: last.focused,
                "reverse Tab escaped rather than reaching last action",
            )
            assert_visible(last, current, "keyboard-focused last action")
            record["keyboard_wrap_reveals_last"] = True
            if zoom == 200:
                # A real periodic transport update must not reset the retained
                # scroll offset or start fighting the selected last action.
                time.sleep(5.5)
                assert last.focused
                assert_visible(last, current, "last action after idle transport update")
                record["retained_after_transport_update"] = True
                record["idle"] = observe_idle()
            shell.move(*point(last))
            shell.screenshot(args.output / f"context-menu-{width}-{zoom}-last.png")
            shell.key("Return")
            cancel_prompt()
            record["last_keyboard_activation"] = True
            # Wheel movement plus real pointer activation exercises both ends;
            # focusing a clipped AT-SPI node is not used to fake visibility.
            current = open_user()
            first = control("Profile and connection", current)
            before_background = target_row().extents()
            shell.wheel(*point(current), -60)
            assert_visible(first, current, "pointer first action")
            time.sleep(0.2)
            assert target_row().extents() == before_background, (
                "popup wheel scrolled background channel tree"
            )
            shell.click(*point(first))
            close_profile()
            record["first_pointer_activation"] = True
            current = open_user()
            last = control("Ban…", current)
            before_background = target_row().extents()
            shell.wheel(*point(current), 80)
            assert_visible(last, current, "pointer last action")
            time.sleep(0.2)
            assert target_row().extents() == before_background, (
                "popup wheel escaped at its scroll boundary"
            )
            shell.click(*point(last))
            cancel_prompt()
            record["background_wheel_isolation"] = True
            record["last_pointer_activation"] = True
            current = open_user()
            first = control("Profile and connection", current)
            shell.key("Tab")
            eventually(
                lambda first=first: first.focused,
                "first action not keyboard focused after reopening",
            )
            assert_visible(first, current, "keyboard first action")
            shell.key("Return")
            close_profile()
            record["first_keyboard_activation"] = True
            current = open_user()
            region = named("Context menu actions", parent=current)
            assert region
            # Test the shared region's explicit reading shortcuts independently
            # of action focus: these scroll only, never activate moderation.
            region.call("Component", "GrabFocus")
            shell.key("End")
            assert_visible(
                control("Ban…", current), current, "End scrolled last action"
            )
            shell.key("Home")
            assert_visible(
                control("Profile and connection", current),
                current,
                "Home scrolled first action",
            )
            shell.key("Page_Down")
            shell.key("Page_Up")
            shell.key("Down")
            shell.key("Up")
            shell.screenshot(args.output / f"context-menu-{width}-{zoom}-first.png")
            close_menu()
            assert menu() is None
            current = open_user()
            g = geometry()
            # Stay inside the application surface, clear of the native resize
            # gutter; a click on the compositor desktop is not a layer event.
            shell.click(g["x"] + 60, g["y"] + 120)
            eventually(lambda: menu() is None, "outside click did not dismiss menu")
            record["reading_keys_and_dismissal"] = True
            if width == 1280:
                message = eventually(
                    lambda: next(
                        (
                            node
                            for node, role, name, _ in items(application)
                            if role == "list item" and "Normal control Nuschl" in name
                        ),
                        None,
                    ),
                    "control message absent",
                )
                x, y, w, h = message.extents()
                g = geometry()
                shell.click(g["x"] + x + w - 3, g["y"] + y + h - 3, button=3)
                current = eventually(menu, "right-edge message menu absent")
                assert inside(current.extents(), viewport()), (
                    "message popup escaped right window edge"
                )
                assert named("Copy message", "menu item", current)
                record["right_edge_message_popup"] = current.extents()
                close_menu()
        prompt_editing = check_prompt_editing()
        frames = args.output / "frames.log"
        if frames.exists():
            # Drain the collector's last completed draw without changing its
            # original timestamp; the idle interval has already ended.
            g = geometry()
            shell.place("Nuschl", g["x"], g["y"], 901, 800)
            time.sleep(0.3)
            origin = re.search(
                r"origin Instant \{ tv_sec: (\d+), tv_nsec: (\d+) \}",
                frames.read_text(),
            )
            assert origin
            origin_us = int(origin[1]) * 1_000_000 + int(origin[2]) // 1000
            idle = next((row["idle"] for row in records if row.get("idle")), None)
            if idle:
                events = []
                for line in frames.read_text().splitlines()[1:]:
                    fields = line.split()
                    at = origin_us + int(fields[1])
                    if idle["start_us"] <= at <= idle["end_us"]:
                        events.append(
                            {
                                "kind": fields[0],
                                "at_us": at,
                                "values": list(map(int, fields[2:])),
                            }
                        )
                (args.output / "idle-events.json").write_text(
                    json.dumps(events, indent=2) + "\n"
                )
        return {
            "passed": True,
            "cases": records,
            "private_stock_murmur": True,
            "no_actual_moderation": True,
            "normal_chat_control": True,
            "prompt_editing": prompt_editing,
        }
    finally:
        target.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expect-clipped", action="store_true")
    parser.add_argument(
        "--only-prompt",
        action="store_true",
        help="diagnose input routing independently of geometry",
    )
    args = parser.parse_args()
    args.document_links, args.expect_markers = True, False
    fixture.WELCOME_EXTRA = ""
    fixture.seed_description, fixture.configure_handler, fixture.run_actions = (
        seed,
        configure,
        actions,
    )
    welcome_html.run(args)
    proof = json.loads((args.output / "proof.json").read_text())
    proof["context_menus"] = proof.pop("document_links")
    (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")


if __name__ == "__main__":
    main()
