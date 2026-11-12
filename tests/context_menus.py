#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Private native admin-menu reachability at narrow sizes and high zoom."""

import argparse
import json
import sqlite3
import time
from pathlib import Path

import document_link_regression as fixture
import welcome_html
from accessibility import items
from layout_regression import inside
from local_murmur import eventually
from mumble_client import Client

TARGET = "Menu target " + "LongUnbrokenName" * 7 + "日本語"


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

    def named(name, role=None, parent=application):
        return next(
            (
                node
                for node, kind, label, _ in items(parent)
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

    def open_user():
        row = eventually(
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
        shell.click(*point(row), button=3)
        current = eventually(menu, "user menu missing")
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

    def cancel_prompt(check_text=False):
        dialog = eventually(
            lambda: named("Input", "dialog"), "Ban action did not open reason prompt"
        )
        if check_text:
            reason = eventually(
                lambda: next(
                    (
                        node
                        for node, role, name, _ in items(dialog)
                        if role in ("entry", "text") and name.startswith("Ban ")
                    ),
                    None,
                ),
                "reason input missing",
            )
            reason.call("Component", "GrabFocus")
            shell.type("sample")
            shell.key("Home")
            shell.type("A")
            shell.key("End")
            shell.type("B")
            eventually(
                lambda: reason.text == "AsampleB",
                "menu navigation stole prompt text editing",
            )
        eventually(
            lambda: named("Cancel", "push button", dialog), "Cancel missing"
        ).press()
        eventually(
            lambda: named("Input", "dialog") is None, "Cancel did not dismiss prompt"
        )
        assert target.process.poll() is None, "fixture target was actually moderated"

    def close_profile():
        eventually(
            lambda: named("Close", "push button"), "profile close missing"
        ).press()
        eventually(
            lambda: named("Close", "push button") is None,
            "profile remained after close",
        )

    try:
        shell.key("Shift_L")
        for width, height, zoom, steps in [
            (1280, 800, 100, 0),
            (900, 600, 100, 0),
            (900, 600, 200, 5),
        ]:
            close_menu()
            shell.chord("Control_L", "0")
            shell.place("Nuschl", 40, 40, width, height)
            for _ in range(steps):
                shell.chord("Control_L", "equal")
            time.sleep(0.3)
            current = open_user()
            entries = [
                name for _, role, name, _ in items(current) if role == "menu item"
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
            shell.move(*point(last))
            shell.screenshot(args.output / f"context-menu-{width}-{zoom}-last.png")
            shell.key("Return")
            cancel_prompt(check_text=zoom == 200)
            record["last_keyboard_activation"] = True
            # Wheel movement plus real pointer activation exercises both ends;
            # focusing a clipped AT-SPI node is not used to fake visibility.
            current = open_user()
            first = control("Profile and connection", current)
            shell.wheel(*point(current), -60)
            assert_visible(first, current, "pointer first action")
            shell.click(*point(first))
            close_profile()
            record["first_pointer_activation"] = True
            current = open_user()
            last = control("Ban…", current)
            shell.wheel(*point(current), 80)
            assert_visible(last, current, "pointer last action")
            shell.click(*point(last))
            cancel_prompt()
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
            shell.click(g["x"] + g["width"] - 15, g["y"] + 70)
            eventually(lambda: menu() is None, "outside click did not dismiss menu")
            record["reading_keys_and_dismissal"] = True
        return {
            "passed": True,
            "cases": records,
            "private_stock_murmur": True,
            "no_actual_moderation": True,
            "normal_chat_control": True,
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
