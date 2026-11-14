#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Dialogs end at their content (#198): the screenshots.py scene dialog-overscroll.

At 100 % and 200 % zoom, Settings and Ada's "Profile and connection" (Details) are
scrolled to the end with End and with a wheel over the scrollbar gutter. The lowest
accessible control must sit within the bottom padding, and no accessible text may
have a run of three or more spaces. A build without the fix ends on a blank page.
"""
import json
import re
import time

from accessibility import items
from local_murmur import eventually

# The dialogs' own bottom padding is 20 to 25 logical px; a blank page is a
# whole viewport (hundreds of px).
PADDING = 32


def settled(dialog):
    """The dialog's and its descendants' extents once smooth scrolling stops."""
    def extents():
        found, stack = [], [dialog]
        while stack:
            for child in stack.pop().children():
                if all(size > 0 for size in child.extents()[2:]):
                    found.append(child.extents())
                stack.append(child)
        return dialog.extents(), found
    previous = None
    for _ in range(30):
        current = extents()
        if current == previous:
            return current
        previous = current
        time.sleep(0.4)
    raise AssertionError("the dialog did not stop scrolling")


def run(args, shell, registry, profile, shot):
    assert registry, "dialog-overscroll needs --at-spi"
    app = eventually(lambda: next((n for n in registry.children() if n.name.lower() == "nuschl"), None),
                     "AT-SPI desktop absent")

    def find(role, name, prefix=False):
        return eventually(lambda: next((n for n, r, text, _ in items(app) if r == role
                                        and (text.startswith(name) if prefix else text == name)), None),
                          f"missing {role} {name!r}")

    def on_screen(box, dx, dy):
        origin = json.loads(shell.eval(
            "JSON.stringify((()=>{const b=global.get_window_actors().map(a=>a.meta_window)"
            ".find(w=>w.get_title().startsWith('Nuschl')).get_buffer_rect();return [b.x,b.y];})())"))
        return origin[0] + box[0] + dx, origin[1] + box[1] + dy

    def end_gap(dialog, how, zoom):
        """Scrolls from the top to the end; the space below the lowest control."""
        shell.key("Home")
        settled(dialog)
        if how == "key":
            shell.key("End")
        else:
            box = dialog.extents()
            for _ in range(4):
                shell.wheel(*on_screen(box, box[2] - round(6 * zoom), box[3] // 2), 40)
                time.sleep(0.3)
        (_, y, _, height), controls = settled(dialog)
        return y + height - max(top + tall for _, top, _, tall in controls)

    results = []
    for zoom, steps in [(1.0, 0), (2.0, 5)]:
        shell.chord("Control_L", "0")
        for _ in range(steps):
            shell.chord("Control_L", "equal")
            time.sleep(0.1)
        eventually(lambda: abs(json.loads((profile / "config/settings.json").read_text())["appearance"]["zoom"]
                               - zoom) < .001, "zoom not saved")
        for name in ("Settings", "Details"):
            if name == "Settings":
                find("push button", "Settings").press()
            else:
                tree, peer = find("tree", "Channels"), find("tree item", "Ada Lovelace", prefix=True)
                # At 200 % Ada's row is below the sidebar's fold: wheel it into view.
                for _ in range(10):
                    (_, top, _, tall), (_, y, _, h) = tree.extents(), peer.extents()
                    if top <= y and y + h <= top + tall:
                        break
                    shell.wheel(*on_screen(tree.extents(), 20, tall // 2), 1 if y + h > top + tall else -1)
                    time.sleep(0.5)
                box = peer.extents()
                shell.click(*on_screen(box, box[2] // 2, box[3] // 2), button=3)
                find("menu item", "Profile and connection").press()
            dialog = find("dialog", name)
            spaced = [text for _, _, text, _ in items(app) if re.search(r"\S {3,}\S", text)]
            gaps = {how: end_gap(dialog, how, zoom) for how in ("key", "wheel")}
            shot(f"overscroll-{name.lower()}-{int(zoom * 100)}")
            results.append({"dialog": name, "zoom": zoom, "gap_below_last": gaps, "spaced_text": spaced})
            shell.key("Escape")
            eventually(lambda: not any(role == "dialog" for _, role, _, _ in items(app)), f"{name} did not close")
    shell.chord("Control_L", "0")
    (args.output / "dialog-overscroll.json").write_text(json.dumps(results, indent=2) + "\n")
    failures = [result for result in results if result["spaced_text"]
                or max(result["gap_below_last"].values()) > PADDING * result["zoom"]]
    assert not failures, f"blank space below the last control or spaced text: {failures}"
    print(f"Dialog overscroll passed: {json.dumps(results)}", flush=True)
