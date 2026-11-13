#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Native scrollbar proof in the existing owned ACL/GNOME/Murmur fixture.

Accepts acl_editor.py's arguments. Uses a fresh fixture profile, private
compositor and loopback stock server. No host display, clipboard or devices.
Tests pixels and real pointer/keyboard input, not injected widget state.
"""
import json
import os
import sqlite3
import subprocess
import time
import uuid

import acl_editor as fixture
from accessibility import items, snapshot
from admin_bootstrap import encode
from hash_document_wire import fields
from layout_regression import inside
from local_murmur import eventually


_configure = fixture.configure


class ScrollMurmur(fixture.OwnedMurmur):
    def seed(self):
        super().seed()
        with sqlite3.connect(self.database) as database:
            database.executemany(
                "INSERT INTO channels (server_id, channel_id, parent_id, name, inheritacl)"
                " VALUES (1, ?, 0, ?, 1)",
                [(100 + index, f"Scrollbar room {index:02}") for index in range(20)],
            )


def configure(shell, root, output):
    env = _configure(shell, root, output)
    path = root / "profile/config/servers.json"
    bookmarks = json.loads(path.read_text())
    # Normal fresh-profile catalog entries, pointing only at this owned server.
    # They never auto-connect and require no unrelated loopback service.
    bookmarks.extend(dict(bookmarks[0], id=str(uuid.uuid4()), name=f"Scrollbar bookmark {i:02}",
                          username=f"Unused scrollbar {i}", auto_connect=False) for i in range(18))
    path.write_text(json.dumps(bookmarks))
    return env


def actions(args, root, shell, application, bot, _murmur):
    shell.key("Shift_L")
    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]
    admin = fixture.operator(root, pin)
    try:
        acl = admin.acl()
        rules = [raw for raw in acl.get(4, []) if not fields(raw).get(3, [1])[0]]
        for index in range(len(rules), 128):
            rules.append(encode({1: [1], 2: [1], 3: [0],
                                 5: [f"scroll-rule-{index:03}".encode()], 6: [0], 7: [0]}))
        groups = []
        for raw in acl.get(3, []):
            group = fields(raw)
            group.pop(7, None)
            groups.append(encode(group))
        for index in range(len(groups), 128):
            groups.append(encode({1: [f"scroll-group-{index:03}".encode()],
                                  2: [0], 3: [1], 4: [1]}))
        acl[3], acl[4], acl[5] = groups, rules, [0]
        admin.send(13, encode(acl))
        checked = admin.acl()
        first_rule = "@" + fields(checked[4][0])[5][0].decode()
        last_rule = "@" + fields(checked[4][-1])[5][0].decode()
        last_group = "@" + fields(checked[3][-1])[1][0].decode()
    finally:
        admin.close()

    def named(name, role=None, prefix=False):
        return next((node for node, kind, label, _ in items(application)
                     if (not role or kind == role)
                     and (label.startswith(name) if prefix else label == name)), None)

    def wait(name, role=None, prefix=False):
        return eventually(lambda: named(name, role, prefix), f"missing {name}")

    def origin():
        return json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors()"
            ".map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));"
            "const b=w.get_buffer_rect();return [b.x,b.y];})())"))

    def point(node, right=None, fraction=.5):
        ox, oy = origin()
        x, y, width, height = node.extents()
        return (ox + x + (width // 2 if right is None else width - right),
                oy + y + round(height * fraction))

    def focus(node, within=None):
        assert node.call("Component", "GrabFocus")[0]
        if within:
            eventually(lambda: node.focused and inside(node.extents(), within.extents()),
                       f"focused {node.name} was not revealed")
        time.sleep(.15)

    def park_pointer():
        # Deliver a real leave move inside the app. Leaving the Wayland surface
        # entirely need not dispatch another mouse move to dismiss a tooltip.
        ox, oy = origin()
        shell.move(ox + 100, oy + 20)

    def bar(node, zoom, name, visible=True, inset=0):
        # Independently locate the painted thumb in its reserved gutter. Read
        # only the owned screenshot; no UI-internal scroll offset is consulted.
        park_pointer()
        time.sleep(.3)
        shot = args.output / f"{name}-{zoom}.png"
        shell.screenshot(shot)
        ox, oy = origin()
        x, y, width, height = node.extents()
        gutter = max(8, round(8 * zoom / 100))
        margin = round(inset * zoom / 100)
        crop = (ox + x + width - gutter - margin, oy + y, gutter, height)
        raw = subprocess.run([os.environ.get("MAGICK", "magick"), str(shot), "-crop",
            f"{crop[2]}x{crop[3]}+{crop[0]}+{crop[1]}", "+repage", "-depth", "8", "rgb:-"],
            check=True, capture_output=True, timeout=30).stdout
        assert len(raw) == gutter * height * 3, (crop, len(raw))
        thumb = (0x35, 0x3e, 0x4e)  # shared dark-theme BORDER_STRONG
        rows = []
        for row in range(height):
            pixels = [tuple(raw[(row * gutter + col) * 3:(row * gutter + col + 1) * 3])
                      for col in range(gutter)]
            if sum(all(abs(a-b) <= 2 for a, b in zip(pixel, thumb)) for pixel in pixels) >= max(1, round(2 * zoom / 100)):
                rows.append(row)
        if not visible:
            # Rounded container corners can share the border/normal-thumb
            # colour. The straight gutter must have no overflowing-axis paint.
            corner = round(12 * zoom / 100)
            assert not [row for row in rows if corner < row < height-corner], (name, rows)
            return {"bounds": node.extents(), "painted_thumb": None, "png": shot.name}
        assert rows, f"no visible native thumb: {name}, {crop}"
        # A border alone is not a thumb: require a bounded segment on a long list.
        assert len(rows) < height * .8, (name, rows, height)
        middle = rows[len(rows) // 2]
        columns = [col for col in range(gutter) if all(abs(a-b) <= 2 for a, b in zip(
            raw[(middle * gutter + col) * 3:(middle * gutter + col + 1) * 3], thumb))]
        # The kit insets its thumb from the track edge. A guessed right4px can
        # hit the track outside the thumb and legitimately jump without dragging.
        right = gutter + margin - (min(columns) + max(columns)) / 2
        return {"bounds": node.extents(), "painted_thumb": [min(rows), max(rows)],
                "thumb_x_from_right": right, "png": shot.name}

    def input_value(node, name):
        # Native Entry currently has no AT-SPI Text interface. Locate by its
        # accessible name, then read only its rendered value as other fixtures
        # do for text with no accessible counterpart. Never use the clipboard.
        # Remove the blinking caret before OCR; its vertical stroke can merge
        # with the last glyph. Dialog focus leaves the revealed viewport still.
        # Virtual keyboard injection returning does not mean the Wayland client
        # has consumed its final character. Settle before changing focus.
        time.sleep(.25)
        assert dialog.call("Component", "GrabFocus")[0]
        park_pointer()
        time.sleep(.2)
        shot = args.output / f"{name}.png"
        shell.screenshot(shot)
        ox, oy = origin()
        x, y, width, height = node.extents()
        png = subprocess.run([os.environ.get("MAGICK", "magick"), str(shot), "-crop",
            f"{width-2}x{height-2}+{ox+x+1}+{oy+y+1}", "+repage", "-resize", "200%", "png:-"],
            capture_output=True, check=True, timeout=30).stdout
        value = subprocess.run([os.environ.get("TESSERACT", "tesseract"), "stdin", "stdout", "--psm", "7"],
            input=png, capture_output=True, check=True, timeout=30).stdout.decode().strip()
        return value.replace(" ", "").rstrip("|")

    records = []
    for width, height, zoom, steps in [(1280, 800, 100, 0), (900, 700, 125, 2),
                                      (1000, 800, 150, 3), (1000, 800, 200, 5)]:
        shell.chord("Control_L", "0")
        shell.place("Nuschl", 40, 40, width, height)
        for _ in range(steps):
            shell.chord("Control_L", "equal")
            time.sleep(.1)
        time.sleep(.3)
        channel = wait("Root", "tree item", prefix=True)
        focus(channel)
        shell.click(*point(channel), button=3)
        wait("Edit permissions…", "menu item").press()
        wait("Rules · 128", prefix=True)
        dialog = wait("Details", "dialog")
        owner = wait("ACL rules", "list box")
        focus(owner, dialog)
        shell.key("Home")
        initial = wait(first_rule, "push button")
        record = {"zoom": zoom, "window": [width, height], "rules": bar(owner, zoom, "rules-top")}
        before = owner.extents()
        # One long wheel run and then another step at the edge must not move
        # the containing dialog or an adjacent list under the parked pointer.
        shell.wheel(*point(owner), 180)
        wait(last_rule, "push button")
        shell.wheel(*point(owner), 3)
        time.sleep(.25)
        assert owner.extents() == before, "nested wheel escaped into the dialog"
        record["rules_end"] = bar(owner, zoom, "rules-end")
        assert record["rules_end"]["painted_thumb"][0] > record["rules"]["painted_thumb"][0]
        # Grab during opposite wheel easing, then return to the first item by
        # dragging the actual native thumb; no synthetic scrollbar state.
        shell.wheel(*point(owner, right=record["rules_end"]["thumb_x_from_right"]), -1)
        end = record["rules_end"]["painted_thumb"]
        grab = (end[0] + end[1]) / 2 / owner.extents()[3]
        shell.drag(point(owner, right=record["rules_end"]["thumb_x_from_right"], fraction=grab),
                   point(owner, right=record["rules_end"]["thumb_x_from_right"], fraction=0.01))
        wait(first_rule, "push button")
        time.sleep(.3)
        assert named(first_rule, "push button"), "wheel easing resumed after a thumb grab"
        # The capture canvas must not block ordinary row actions.
        target = wait("@scroll-rule-005", "push button")
        shell.click(*point(target))
        target_field = eventually(lambda: next((node for node, role, name, _ in items(application)
            if name == "ACL rule target" and role == "entry"), None), "rule input absent")
        focus(target_field, dialog)
        eventually(lambda: input_value(target_field, f"selected-rule-{zoom}") == "scroll-rule-005",
                   "canvas intercepted a rule pointer selection")
        wait("Remove rule", "push button")
        focus(owner, dialog)
        shell.key("End")
        final = wait(last_rule, "push button")
        assert inside(final.extents(), dialog.extents())
        groups_owner = wait("ACL groups", "list box")
        focus(groups_owner, dialog)
        shell.key("Home")
        record["groups"] = bar(groups_owner, zoom, "groups-top")
        shell.key("End")
        wait(last_group, "push button")
        # Inputs keep their own selection/Home/End rather than scrolling their
        # containing viewport. No clipboard operations are used.
        field = eventually(lambda: next((node for node, role, name, _ in items(application)
            if name == "New group name" and role in ("entry", "text")), None), "group input absent")
        focus(field, dialog)
        # Keep the revealed viewport but move focus away: the subsequent real
        # pointer click, not GrabFocus, must put the caret in the input.
        focus(dialog)
        shell.click(*point(field))
        eventually(lambda: field.focused, "canvas intercepted input pointer focus")
        shell.type("sample")
        time.sleep(.2)
        key_bounds = field.extents()
        for key, text in [("Home", "A"), ("End", "B"), ("Left", "X"), ("Right", "Y")]:
            shell.key(key)
            time.sleep(.15)
            shell.type(text)
            time.sleep(.15)
        assert field.focused and field.extents() == key_bounds, "editing keys moved the viewport"
        edited = input_value(field, f"editing-keys-{zoom}")
        assert edited == "AsampleXBY", edited
        record["input_edit_keys"] = edited
        shell.drag(point(field, right=field.extents()[2]),
                   point(field, right=4 * zoom / 100))
        shell.type("selected-by-pointer")
        eventually(lambda: input_value(field, f"selected-input-{zoom}") == "selected-by-pointer",
                   "canvas intercepted input text selection")
        done = wait("Done", "push button")
        focus(done, dialog)
        # The outer bar remains painted after its content has moved.
        record["dialog"] = bar(dialog, zoom, "modal-end")
        done.press()
        eventually(lambda: not named("Details", "dialog"), "ACL did not close")
        records.append(record)
        (args.output / "native-scrollbars.json").write_text(json.dumps(records, indent=2) + "\n")

    # Generic settings nested lists use the same visible pattern as virtual ACL
    # lists. Exercise ordinary focus reveal and their scrollbar while clipped.
    shell.chord("Control_L", "0")
    shell.place("Nuschl", 40, 40, 900, 700)
    wait("Settings", "push button").press()
    dialog = wait("Settings", "dialog")
    opener = wait("Interface font:", "push button", prefix=True)
    focus(opener)
    opener.press()
    fonts = wait("Installed fonts")
    choices = [node for _, node, role, _ in snapshot(fonts) if role == "radio button"]
    assert len(choices) > 1, "private fixture needs an overflowing font list"
    focus(choices[-1], dialog)
    eventually(lambda: inside(fonts.extents(), dialog.extents()), "nested font viewport not revealed")
    font_bar = bar(fonts, 100, "fonts")
    before = fonts.extents()
    shell.wheel(*point(fonts, right=font_bar["thumb_x_from_right"]), -80)
    time.sleep(.3)
    assert fonts.extents() == before, "font gutter wheel escaped into the dialog"
    records.append({"settings_fonts": font_bar, "settings": bar(dialog, 100, "settings"),
                    "font_gutter_wheel_isolated": True})
    wait("Done", "push button").press()
    eventually(lambda: not named("Settings", "dialog"), "Settings did not close")
    records.append({"rail": bar(wait("Servers", "list"), 100, "rail"),
                    "channels": bar(wait("Channels"), 100, "channels")})
    shell.chord("Control_L", "k")
    finder = wait("Quick switcher", "dialog")
    shell.type("Scrollbar room")
    choices = wait("20 results", "list box")
    records.append({"finder": bar(choices, 100, "finder", inset=8)})
    shell.key("Escape")
    eventually(lambda: not named("Quick switcher", "dialog"), "finder did not close")
    shell.chord("Control_L", "period")
    picker = wait("Emoji", "dialog")
    records.append({"emoji": bar(picker, 100, "emoji")})
    shell.type("thumbs up")
    time.sleep(.3)
    records.append({"emoji_short": bar(picker, 100, "emoji-short", visible=False)})
    shell.key("Escape")
    for index in range(14):
        bot.send(kind="text", channel=0, body=f"scrollbarprobe message {index:02}")
        time.sleep(1.05)
    shell.chord("Control_L", "f")
    wait("Search messages", "entry")
    shell.type("scrollbarprobe")
    results = wait("14 results", "list")
    records.append({"search": bar(results, 100, "search")})
    shell.key("Escape")
    # Open a real context menu from a visible row; no stale off-clip coordinates.
    # Its occluding surface must keep wheel events away from background panels.
    for zoom, steps in [(100, 0), (200, 5)]:
        shell.chord("Control_L", "0")
        shell.place("Nuschl", 40, 40, 1280 if zoom == 100 else 900, 800)
        for _ in range(steps):
            shell.chord("Control_L", "equal")
            time.sleep(.1)
        channels = wait("Channels")
        shell.wheel(*point(channels), -80)
        time.sleep(.3)
        user = wait("Control sender", "tree item", prefix=True)
        assert inside(user.extents(), channels.extents()), "context target not visible"
        before = user.extents()
        shell.click(*point(user), button=3)
        menu = wait("Context menu", "menu")
        shell.wheel(*point(menu), 80)
        time.sleep(.3)
        assert user.extents() == before, "context wheel scrolled background channel tree"
        record = {"context_menu_zoom": zoom, "bounds": menu.extents(), "wheel_isolated": True}
        if zoom == 200:
            record["bar"] = bar(menu, zoom, "context-menu", inset=5)
        records.append(record)
        shell.key("Escape")
    (args.output / "native-scrollbars.json").write_text(json.dumps(records, indent=2) + "\n")
    return {"native_scrollbars": records, "private_loopback_only": True}


if __name__ == "__main__":
    fixture.actions = actions
    fixture.configure = configure
    fixture.OwnedMurmur = ScrollMurmur
    fixture.main()
