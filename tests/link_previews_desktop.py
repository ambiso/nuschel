#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Production native preview UI against synthetic peers in the dedicated VM.

No host networking/device/profile is authorized by this script. The VM driver
sets public-numbered loopback aliases and denies every non-loopback output.
`peers` reuses the frozen native-fetch fixture's HTTP/SOCKS implementation;
`native-peers` runs those same peers with only the guest's low-port capability;
`desktop` reuses the private GNOME/AT-SPI/null-Pulse welcome fixture. This tests
legacy one-message HTML fallback, not typed preview/1 or MLS completion.
"""
import argparse
import hashlib
import html
import json
import os
import re
from pathlib import Path
import socket
import socketserver
import signal
import struct
import subprocess
import threading
import time
import types
import uuid


def control(operation, port=39211):
    from link_preview_fetch import exact
    payload = json.dumps({"operation": operation}).encode()
    with socket.create_connection(("127.0.0.1", port), timeout=3) as stream:
        stream.settimeout(3)
        stream.sendall(struct.pack("!I", len(payload)) + payload)
        size = struct.unpack("!I", exact(stream, 4))[0]
        assert size <= 512 * 1024
        return json.loads(exact(stream, size))


def peers(args):
    import link_preview_fetch as peer
    import zlib
    directory = args.directory.resolve(strict=True)
    assert directory.is_dir() and directory.stat().st_uid == os.getuid()
    # Refuse an accidental host invocation. The driver creates this marker
    # only inside its fresh guest, with the exact guest network identity.
    identity = json.loads((directory / "vm-identity.json").read_text())
    assert identity["net_inode"] == os.stat("/proc/self/ns/net").st_ino
    assert Path("/run/nuschl-preview-dedicated-vm").is_file()
    routes = subprocess.check_output(["ip", "route", "show", "table", "all"], text=True, timeout=3)
    assert "default" not in routes, routes
    for address in (peer.PAGE, peer.DOH, peer.PROXY_SOURCE):
        assert f"local {address} " in routes, routes

    # The GUI pixel oracle uses a visible 80×40 image at its decoded size.
    # This process owns the GUI peers; native-peers loads its original image.
    width, height = 80, 40
    peer.PNG = (b"\x89PNG\r\n\x1a\n"
        + peer.png_chunk(b"IHDR", struct.pack("!IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + peer.png_chunk(b"IDAT", zlib.compress((b"\x00" + b"\x20\x80\xc0\xff" * width) * height))
        + peer.png_chunk(b"IEND", b""))
    image_identity = dict(width=width, height=height, rgba_bytes=width * height * 4,
        png_bytes=len(peer.PNG), sha256=hashlib.sha256(peer.PNG).hexdigest())

    class State(peer.State):
        def __init__(self):
            super().__init__(directory)
            self.gate_lock = threading.Lock()
            self.gate_next = False
            self.release_http = threading.Event()
            self.release_http.set()

        def snapshot(self):
            return super().snapshot() | {"gui_image": image_identity}

        def event(self, kind, **values):
            super().event(kind, **values)
            if kind == "http" and values.get("path") == "/page":
                with self.gate_lock:
                    gate = self.gate_next
                    self.gate_next = False
                if gate:
                    super().event("http_gate_entered")
                    assert self.release_http.wait(8), "bounded HTTP gate was not released"

    class Gate(socketserver.BaseRequestHandler):
        def handle(self):
            self.request.settimeout(3)
            size = struct.unpack("!I", peer.exact(self.request, 4))[0]
            assert size <= 128
            value = json.loads(peer.exact(self.request, size))
            if value == {"operation": "arm_http"}:
                with state.gate_lock:
                    state.gate_next = True
                    state.release_http.clear()
            elif value == {"operation": "release_http"}:
                state.release_http.set()
            else:
                raise ValueError("unknown fixed gate action")
            self.request.sendall(struct.pack("!I", 2) + b"{}")

    state = State()
    servers = [peer.Server((address, port), handler, state) for address, port, handler in (
        (peer.PAGE, 80, peer.Http), (peer.PAGE, 443, peer.Http),
        (peer.DOH, 443, peer.Http), ("127.0.0.1", 80, peer.Http),
        ("127.0.0.1", 1080, peer.Socks), ("127.0.0.1", peer.CONTROL, peer.Control),
        ("127.0.0.1", peer.CONTROL + 1, Gate),
    )]
    threads = [threading.Thread(target=server.serve_forever) for server in servers]
    threads.append(threading.Thread(target=peer.dns, args=(state,)))
    for thread in threads:
        thread.start()
    (directory / "ready.json").write_text(json.dumps({"pid": os.getpid(), "routes": routes}) + "\n")
    try:
        assert state.stop.wait(720), "peer service exceeded its bounded VM lifetime"
    finally:
        state.stop.set()
        state.release_dns.set()
        state.release_http.set()
        for server in servers:
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(8)
            assert not thread.is_alive()
        (directory / "peers.json").write_text(json.dumps(state.snapshot(), indent=2) + "\n")


def native_peers(args):
    """Fixed native-test peers; the guest root already configured the aliases."""
    import link_preview_fetch as peer
    directory = args.directory.resolve(strict=True)
    assert directory.is_dir() and directory.stat().st_uid == os.getuid()
    assert os.getuid() != 0 and os.getgid() != 0
    assert Path("/run/nuschl-preview-dedicated-vm").is_file()
    with (directory / "vm-identity.json").open() as stream:
        identity_text = stream.read(4097)
    assert len(identity_text) <= 4096
    identity = json.loads(identity_text)
    assert identity["net_inode"] == os.stat("/proc/self/ns/net").st_ino
    assert not any((directory / name).exists() for name in ("server.pem", "server-key.pem", "peers-ready.json"))
    with Path("/proc/self/status").open() as stream:
        status = stream.read(16 * 1024 + 1)
    assert len(status) <= 16 * 1024
    authority = {name: next(line.split(":", 1)[1].strip() for line in status.splitlines()
                            if line.startswith(name + ":"))
                 for name in ("CapEff", "CapPrm", "CapAmb", "NoNewPrivs")}
    assert all(int(authority[name], 16) == 1 << 10 for name in ("CapEff", "CapPrm", "CapAmb")), authority
    assert authority["NoNewPrivs"] == "1"
    routes = subprocess.check_output(["ip", "route", "show", "table", "all"], text=True, timeout=3)
    assert len(routes) <= 16 * 1024 and "default" not in routes, routes
    for address in (peer.PAGE, peer.DOH, peer.PROXY_SOURCE):
        assert f"local {address} " in routes, routes
    state = peer.State(directory)
    servers = [peer.Server((address, port), handler, state) for address, port, handler in (
        (peer.PAGE, 80, peer.Http), (peer.PAGE, 443, peer.Http), (peer.DOH, 443, peer.Http),
        ("127.0.0.1", 80, peer.Http), ("127.0.0.1", 1080, peer.Socks),
        ("127.0.0.1", peer.CONTROL, peer.Control),
    )]
    threads = [threading.Thread(target=server.serve_forever) for server in servers]
    threads.append(threading.Thread(target=peer.dns, args=(state,)))
    for thread in threads:
        thread.start()
    (directory / "peers-ready.json").write_text(json.dumps(dict(
        pid=os.getpid(), net_inode=identity["net_inode"], routes=routes, authority=authority,
        scope="Dedicated VM native peers; only NET_BIND_SERVICE, no test outcome implied")) + "\n")
    try:
        assert state.stop.wait(180), "native peer service exceeded its bounded VM lifetime"
    finally:
        state.stop.set()
        state.release_dns.set()
        for server in servers:
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(8)
            assert not thread.is_alive()
        (directory / "peers.json").write_text(json.dumps(state.snapshot(), indent=2) + "\n")


def desktop(args):
    import document_link_regression as scenario
    import welcome_html
    from accessibility import Node, items
    from local_murmur import eventually
    from local_nuschld import Nuschld

    assert Path("/run/nuschl-preview-dedicated-vm").is_file(), "use the owned VM fixture"
    args.output.mkdir(parents=True, exist_ok=False)
    args.output.chmod(0o700)
    cases = []
    extra_children = []
    scene_start = len(control("snapshot")["events"])
    def digest(path):
        value = hashlib.sha256()
        with path.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                value.update(block)
        return value.hexdigest()
    programs = {}
    for directory in (args.binaries, args.binaries.parent / "libexec/nuschl"):
        entries = sorted(directory.iterdir()) if directory.is_dir() else []
        assert len(entries) <= 64
        for path in entries:
            if path.is_file():
                assert path.stat().st_size <= 512 * 1024 * 1024
                programs[str(path)] = dict(realpath=str(path.resolve()), sha256=digest(path))
    required = ("nuschl", "nuschl-cli", "nuschld", "nuschl-content-worker")
    assert all((args.binaries / name).is_file() for name in required)
    fixture_hashes = {name: digest(Path(__file__).parent / name) for name in (
        "link_previews_desktop.py", "link_preview_fetch.py", "welcome_html.py", "gnome_session.py",
        "accessibility.py", "local_murmur.py", "local_nuschld.py", "mumble_client.py")}
    (args.output / "identity.json").write_text(json.dumps(dict(
        source_identity=args.source_identity, programs=programs, fixtures=fixture_hashes,
        scope="Commit or store source path supplied by trusted Nix invocation; file hashes independently measured in guest."), indent=2) + "\n")

    def mark(name, **details):
        cases.append(dict(case=name, **details))
        (args.output / "cases.json").write_text(json.dumps(cases, indent=2) + "\n")

    class RetainedDirectory:
        def __init__(self, **_kwargs):
            self.path = args.output / "runtime"
            self.path.mkdir(mode=0o700)
        def __enter__(self):
            return str(self.path)
        def __exit__(self, *_exc):
            # Owned state remains private for failures and history inspection.
            return False

    class Compatible(Nuschld):
        def __init__(self, _binary, root, port):
            super().__init__(args.binaries, root, port, extra='[history]\nguests = "full"')
            self.ini = root / "welcome-bridge.ini"
            self.ini.write_text('welcometext="Welcome to the <b>Nuschl</b> screenshot server."')
            self.database = root / "unused-legacy-database"
        def seed(self):
            super().seed()
            welcome = self.ini.read_text().split('welcometext="', 1)[1].rsplit('"', 1)[0]
            lines = self.config.read_text().splitlines()
            self.config.write_text("\n".join("welcome_text = " + json.dumps(welcome)
                if line.startswith("welcome_text =") else line for line in lines) + "\n")

    def configure(_shell, root, _output):
        # Exercise the missing-file default, not an explicitly configured Off.
        assert not (root / "profile/config/link-previews.json").exists()
        return {}

    def actions(_args, root, shell, application, receiver, _server):
        sender_pid = int(shell.eval("global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl')).get_pid()"))
        registry = Node(application.bus, "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
        before_apps = {node.destination for node in registry.children()}
        recipient_profile = root / "recipient"
        (recipient_profile / "config").mkdir(parents=True)
        bookmarks = json.loads((root / "profile/config/servers.json").read_text())
        bookmarks[0].update(username="Preview recipient", name="Preview recipient fixture")
        (recipient_profile / "config/servers.json").write_text(json.dumps(bookmarks))
        (recipient_profile / "config/settings.json").write_text(json.dumps({"audio": {"transmit": "push_to_talk"}}))
        env = shell.app_environment() | {
            "PULSE_SERVER": f"unix:{shell.runtime}/pulse/native", "PULSE_SINK": "speakers",
            "PULSE_SOURCE": "microphone.monitor", "NUSCHL_AUDIO_BACKEND": "pulseaudio",
            "PIPEWIRE_REMOTE": str(root / "no-pipewire"), "RUST_LOG": "nuschl=info",
        }
        env.pop("NO_AT_BRIDGE", None)
        recipient = subprocess.Popen([str(args.binaries / "nuschl"), "--profile-dir", str(recipient_profile),
            f"mumble://Preview%20recipient@127.0.0.1:{_server.port}/"], env=env,
            stdout=(args.output / "recipient.log").open("w"), stderr=subprocess.STDOUT, start_new_session=True)
        extra_children.append(recipient)
        recipient_ui = eventually(lambda: next((node for node in registry.children()
            if node.destination not in before_apps and node.name.lower() == "nuschl"), None),
            "independent recipient UI missing")
        receiver.wait(lambda event: event.get("kind") == "user" and event["user"]["name"] == "Preview recipient")

        def focused_window_pid():
            return json.loads(shell.eval("JSON.stringify(global.display.focus_window?.get_pid()??null)"))
        def focus_window(pid):
            shell._pointer()
            shell.eval("(()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
                f".find(w=>w.get_pid()==={pid}); if(!w)throw Error('owned window missing');"
                "w.activate(global.get_current_time());return true;})()")
            eventually(lambda: focused_window_pid() == pid, "owned window did not acquire compositor focus")
        def window_rect(pid, frame=False):
            return json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
                f".find(w=>w.get_pid()==={pid});const b=w.{'get_frame_rect' if frame else 'get_buffer_rect'}();"
                "return [b.x,b.y,b.width,b.height];})())"))
        def resize_window(pid, width, height):
            shell.eval("(()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
                f".find(w=>w.get_pid()==={pid});if(w.is_maximized())w.unmaximize();"
                f"w.move_resize_frame(true,40,40,{width},{height});return true;}})()")
            eventually(lambda: window_rect(pid, frame=True)[2:] == [width, height],
                f"owned window actual frame did not reach {width} by {height}")
        def saved_zoom(profile=root / "profile"):
            return json.loads((profile / "config/settings.json").read_text()).get("appearance", {}).get("zoom", 1.0)
        def wait_zoom(percent, profile=root / "profile"):
            eventually(lambda: abs(saved_zoom(profile) - percent / 100) < .001,
                f"actual saved zoom did not reach {percent} percent")
        def image_pixels(pid, path):
            focus_window(pid)
            shell.screenshot(path)
            x, y, width, height = window_rect(pid)
            assert min(x, y) >= 0 and 0 < width <= 1440 and 0 < height <= 900
            assert x + width <= 1440 and y + height <= 900
            raw = subprocess.run([os.environ.get("MAGICK", "magick"), str(path), "-crop",
                f"{width}x{height}+{x}+{y}", "+repage", "-depth", "8", "rgb:-"],
                check=True, capture_output=True, timeout=15).stdout
            assert len(raw) == width * height * 3
            # The synthetic PNG is a solid cyan, unrelated to theme text.
            return sum(all(abs(raw[offset + channel] - target) <= 8
                for channel, target in enumerate((0x20, 0x80, 0xc0))) for offset in range(0, len(raw), 3))
        focus_window(sender_pid)
        last_policy_labels = []
        observation = {"stage": "initial", "last_requested_mode": None, "last_typed_body": None}
        def named(name, role=None):
            return next((node for node, kind, label, _ in items(application)
                if label == name and (role is None or kind == role)), None)
        def wait(name, role=None):
            return eventually(lambda: named(name, role), f"missing preview UI {name}")
        def ready_card():
            card = named("Link preview", "panel")
            if card is None:
                return None
            pending = [(card, 0)]
            while pending:
                node, depth = pending.pop()
                if node.name == "Synthetic preview":
                    return card
                assert depth <= 12
                pending.extend((child, depth + 1) for child in node.children())
            return None
        def labels():
            return [label for _, _, label, _ in items(application)]
        def activate(name, role="push button"):
            observation["last_action"] = name
            node = wait(name, role)
            assert node.call("Component", "GrabFocus")[0]
            if role == "radio button":
                shell.key("space")
            else:
                node.press()
            return node
        def copied_input(select_all=False):
            # The dedicated guest GNOME clipboard observes real editor copy.
            # A fresh sentinel binds each observation to its own Ctrl+C.
            focus_window(sender_pid)
            field = wait("Message")
            assert field.call("Component", "GrabFocus")[0]
            eventually(lambda: field.focused, "message editor did not acquire keyboard focus")
            if select_all:
                shell.chord("Control_L", "a")
            sentinel = f"owned-preview-copy-{uuid.uuid4().hex}"
            St = "imports.gi.St"
            shell.eval(f"{St}.Clipboard.get_default().set_text({St}.ClipboardType.CLIPBOARD, {json.dumps(sentinel)}); 'ok'")
            shell.chord("Control_L", "c")
            def observed():
                shell.eval(f"global._previewCopy = undefined; {St}.Clipboard.get_default().get_text("
                    f"{St}.ClipboardType.CLIPBOARD, (c,t)=>{{global._previewCopy=t;}}); 'ok'")
                value = eventually(lambda: (value if (value := json.loads(shell.eval(
                    "JSON.stringify({done:global._previewCopy!==undefined,text:global._previewCopy??null})")))["done"] else None),
                    "guest clipboard observation did not finish", timeout=10)
                text = value["text"]
                assert isinstance(text, str) and len(text) <= 4096
                return None if text == sentinel else text
            return eventually(observed, "editor copy did not replace the guest clipboard sentinel", timeout=10)
        def type_message(body):
            assert isinstance(body, str) and len(body) <= 4096
            observation.update(stage="type_message", last_typed_body=body,
                actual_typed_body=None, typed_body_verified=False,
                typing_editor_focused=None, typing_focused_window_pid=None)
            focus_window(sender_pid)
            node = wait("Message")
            assert node.call("Component", "GrabFocus")[0]
            eventually(lambda: node.focused, "message editor did not acquire keyboard focus before typing")
            observation.update(typing_editor_focused=node.focused,
                typing_focused_window_pid=focused_window_pid())
            shell.chord("Control_L", "a")
            shell.type(body)
            actual = copied_input(select_all=True)
            observation["actual_typed_body"] = actual
            assert actual == body, "synthetic editor input must retain the complete intended draft"
            observation["typed_body_verified"] = True
            shell.chord("Control_L", "End")
            return node
        def choose(mode):
            observation.update(stage="choose", last_requested_mode=mode, requested_radio_focused_before_space=None)
            observation.pop("requested_radio_focus_error", None)
            activate("Settings")
            group = wait("Link previews")
            def scoped_radio():
                pending, visited = [(group, 0)], 0
                while pending:
                    node, depth = pending.pop()
                    visited += 1
                    assert visited <= 64 and depth <= 12, "bounded preview radio group exceeded"
                    if node.role == "radio button" and node.name == mode:
                        return node
                    pending.extend((child, depth + 1) for child in node.children())
                return None
            node = eventually(scoped_radio, f"missing {mode} radio within Link previews")
            assert node.call("Component", "GrabFocus")[0]
            try:
                observation["requested_radio_focused_before_space"] = node.focused
            except (RuntimeError, ValueError, TypeError, KeyError, IndexError) as error:
                observation["requested_radio_focus_error"] = type(error).__name__
            eventually(lambda: node.focused, f"{mode} radio did not acquire keyboard focus")
            observation["requested_radio_focused_before_space"] = node.focused
            shell.key("space")
            eventually(lambda: not any("Saving preference" in label for label in labels())
                and any(label.startswith(f"Active policy: {mode}.") for label in labels()),
                f"{mode} preference did not complete")
            last_policy_labels[:] = [label[:512] for label in labels()
                if label.startswith("Active policy:")][:4]
            activate("Done")
        def events():
            snapshot = control("snapshot")
            assert not snapshot["overrun"]
            # Cancellation may legitimately produce BrokenPipeError; retain
            # those diagnostics. A synthetic peer's failed assertion is never
            # an acceptable connection failure or successful test observation.
            assertions = [event for event in snapshot["events"]
                if event.get("kind") == "peer_error" and event.get("error") == "AssertionError"]
            assert not assertions, assertions
            return snapshot["events"]
        def sent_by_reader():
            return [event for event in list(receiver.events) if event.get("kind") == "text"
                and event.get("author") == "Welcome reader"]
        def archive_content(message):
            content = message["content"]
            assert isinstance(content, str), "CLI archive content must be a JSON string"
            return content
        def recipient_card(marker, contained=False):
            timelines = [node for node, role, name, _ in items(recipient_ui)
                if role == "list" and name == "Messages in #Root"]
            if not timelines:
                return None
            assert len(timelines) == 1
            timeline = timelines[0]
            def descendants(root_node):
                pending, visited, result, name_bytes = [(root_node, 0)], set(), [], 0
                while pending:
                    node, depth = pending.pop()
                    identity = (node.destination, node.path)
                    assert identity not in visited, "recipient card traversal repeated a node"
                    visited.add(identity)
                    assert len(visited) <= 1024 and depth <= 32, "recipient card traversal exceeded its bounds"
                    assert node.destination == recipient_ui.destination, "recipient card left its application"
                    name = node.name
                    name_bytes += len(name.encode())
                    assert name_bytes <= 64 * 1024, "recipient card labels exceeded their byte budget"
                    result.append((node, node.role, name))
                    children = node.children()
                    assert len(visited) + len(pending) + len(children) <= 1024
                    pending.extend((child, depth + 1) for child in children)
                return result
            rows = [node for node, role, name in descendants(timeline)
                if role == "list item" and marker in name]
            if not rows:
                return None
            assert len(rows) == 1, "recipient marker must identify one message row"
            row = rows[0]
            card = descendants(row)
            # The row's complete accessible label carries styled quote text;
            # child edges bind the carried image to this same logical message.
            quote = "93.184.215.14 Synthetic preview Owned fixture"
            if not any(quote in " ".join(name.split()) for _, _, name in card):
                return None
            images = [node for node, role, name in card
                if role == "image" and name == "Image (click to enlarge)"]
            if not images:
                return None
            assert len(images) == 1, "recipient card must have one carried image"
            bounds = {"row": row.extents(), "image": images[0].extents()}
            viewport = timeline.extents()
            window = window_rect(recipient.pid)
            if contained:
                for x, y, width, height in bounds.values():
                    tx, ty, tw, th = viewport
                    if not (width > 0 and height > 0 and x >= max(tx, 0) and y >= max(ty, 0)
                        and x + width <= min(tx + tw, window[2]) + 1
                        and y + height <= min(ty + th, window[3]) + 1):
                        return None
            return dict(row=[row.destination, row.path], label=row.name[:8192],
                image=[images[0].destination, images[0].path], bounds=bounds,
                timeline=viewport, window=window)

        def send_and_receive(body, preview):
            observation.update(stage="send_and_receive", last_send_preview=preview)
            marker = body.split()[0]
            before = len(sent_by_reader())
            activate("Send")
            event = receiver.wait(lambda event: event.get("kind") == "text"
                and event.get("author") == "Welcome reader" and marker in event.get("body", ""))
            eventually(lambda: named("Send", "push button"), "typed send receipt did not release composer")
            time.sleep(.4)
            assert len(sent_by_reader()) == before + 1, "preview became multiple logical messages"
            wire = event["body"]
            literal = html.unescape(re.sub(r"<[^>]+>", "", wire.split("<br/><blockquote>", 1)[0]))
            assert literal == body, (literal, body)
            assert ("Synthetic preview" in wire) == preview, wire
            assert ("data:image/" in wire) == preview, wire
            assert body.split()[1] in wire, "original target disappeared from the one message"
            eventually(lambda: any(marker in label for _, _, label, _ in items(recipient_ui)), "real recipient did not render original text")
            if preview:
                # Reuse the actual desktop projection in the browser receiver
                # fixture instead of maintaining a second hand-written card.
                if "legacy_preview_wire" not in observation:
                    wire_path = args.output / "legacy-preview-message.html"
                    wire_path.write_text(wire)
                    observation["legacy_preview_wire"] = {
                        "file": wire_path.name,
                        "sha256": hashlib.sha256(wire.encode()).hexdigest(),
                    }
                pixel_count = None
                def recipient_image_painted():
                    nonlocal pixel_count
                    pixel_count = image_pixels(recipient.pid, args.output / "recipient-card.png")
                    return pixel_count > 300
                try:
                    eventually(lambda: recipient_card(marker),
                        "recipient message row did not retain the domain, title, description and image")
                    eventually(recipient_image_painted,
                        "carried image was not actually painted in independent recipient")
                    focus_window(recipient.pid)
                    shell.chord("Control_L", "0")
                    wait_zoom(100, recipient_profile)
                    resize_window(recipient.pid, 900, 800)
                    for _ in range(5):
                        shell.chord("Control_L", "equal")
                        time.sleep(.1)
                    wait_zoom(200, recipient_profile)
                    card = eventually(lambda: recipient_card(marker, contained=True),
                        "complete recipient card left the narrow 200 percent timeline")
                    def narrow_image_painted():
                        nonlocal pixel_count
                        pixel_count = image_pixels(recipient.pid, args.output / "recipient-card-200.png")
                        return pixel_count > 300
                    eventually(narrow_image_painted, "narrow recipient carried image was not actually painted")
                    mark("recipient_quote_card_fields_and_image_fit_at_narrow_real_200_percent",
                        marker=marker, frame=window_rect(recipient.pid, frame=True), zoom=200,
                        card=card, cyan_pixels=pixel_count)
                    shell.chord("Control_L", "0")
                    wait_zoom(100, recipient_profile)
                    resize_window(recipient.pid, 1280, 800)
                except AssertionError as failure:
                    try:
                        capture_failure(failure, recipient_view=True, pixel_count=pixel_count)
                    except BaseException as capture_error:
                        failure.add_note(f"Recipient diagnostic capture failed: {type(capture_error).__name__}"[:128])
                    raise
                focus_window(sender_pid)
                if args.server == "nuschld":
                    chat = eventually(lambda: next((event for event in list(receiver.events)
                        if event.get("kind") == "chat" and marker in archive_content(event["message"])), None),
                        "compatible server did not archive the projected message")
                    request = 16000 + before
                    receiver.send(kind="history", request_id=request, conversation=chat["conversation"], limit=32)
                    page = receiver.wait(lambda event: event.get("kind") == "history_page" and event.get("request_id") == request)
                    archived = [message for message in page["messages"] if marker in archive_content(message)]
                    # The protocol's Format::LegacyHtml value is 2.
                    assert len(archived) == 1 and archived[0]["format"] == 2, archived
                    assert archive_content(archived[0]) == wire
                    mark("compatible_history_keeps_one_legacy_html_message", seq=archived[0]["seq"])
            mark("single_message_received_by_independent_native_ui", preview=preview,
                wire_sha256=hashlib.sha256(wire.encode()).hexdigest())
            return wire

        def capture_failure(failure, *, recipient_view=False, pixel_count=None):
            prefix = "recipient-failure" if recipient_view else "failure"
            diagnostic = dict({} if recipient_view else observation,
                error_type=type(failure).__name__, error=str(failure)[:2048],
                nodes=[], nodes_truncated=False)
            if recipient_view:
                diagnostic["last_cyan_pixels"] = pixel_count
            else:
                diagnostic["last_observed_policy_labels"] = last_policy_labels[:]
            def expired(_signal, _frame):
                raise TimeoutError("preview failure capture exceeded 12 seconds")
            previous_handler = signal.signal(signal.SIGALRM, expired)
            previous_timer = signal.setitimer(signal.ITIMER_REAL, 12)
            started = time.monotonic()
            try:
                if recipient_view:
                    diagnostic["frame"] = window_rect(recipient.pid, frame=True)
                    diagnostic["buffer"] = window_rect(recipient.pid)
                observed = items(recipient_ui if recipient_view else application)
                diagnostic["node_count"] = len(observed)
                diagnostic["nodes_truncated"] = len(observed) > 1024
                retained_bytes = 0
                for node, role, name, states in observed[:1024]:
                    row = dict(role=role[:64], name=name[:256], states=states)
                    if recipient_view and role in ("image", "list", "list item"):
                        try:
                            row["extents"] = node.extents()
                        except (RuntimeError, ValueError, TypeError, KeyError, IndexError) as error:
                            row["extents_error"] = f"{type(error).__name__}: {error}"[:512]
                    retained_bytes += len(json.dumps(row).encode())
                    if retained_bytes > 128 * 1024:
                        diagnostic["nodes_truncated"] = True
                        break
                    diagnostic["nodes"].append(row)
                if not recipient_view:
                    rows = diagnostic["nodes"]
                    diagnostic["preview_labels"] = [row["name"] for row in rows
                        if "preview" in row["name"].lower() or row["name"].startswith("Text only")][:64]
                    diagnostic["policy_labels"] = [row["name"] for row in rows
                        if row["name"].startswith("Active policy:")][:4]
                    diagnostic["send_labels"] = [row["name"] for row in rows
                        if row["name"] in ("Send", "Sending…")]
                    diagnostic["focused"] = [row for row in rows if row["states"] & (1 << 12)][:16]
                    diagnostic["notice_and_status_labels"] = [row["name"] for row in rows
                        if row["role"] == "status bar" or any(term in row["name"].lower()
                            for term in ("notice", "not sent", "changed", "unavailable", "cannot"))][:64]
                    diagnostic["frame"] = window_rect(sender_pid, frame=True)
                    diagnostic["saved_zoom"] = saved_zoom()
                    diagnostic["focused_window_pid"] = focused_window_pid()
                    snapshot = json.dumps(control("snapshot"), separators=(",", ":")).encode()
                    assert len(snapshot) <= 512 * 1024
                    (args.output / "failure-peers.json").write_bytes(snapshot + b"\n")
                shell.screenshot(args.output / f"{prefix}.png")
            except (AssertionError, EOFError, OSError, RuntimeError, ValueError,
                    TypeError, KeyError, IndexError, subprocess.SubprocessError) as error:
                diagnostic["capture_error"] = f"{type(error).__name__}: {error}"[:512]
            finally:
                signal.setitimer(signal.ITIMER_REAL, 0)
                signal.signal(signal.SIGALRM, previous_handler)
                if previous_timer[0]:
                    signal.setitimer(signal.ITIMER_REAL,
                        max(.001, previous_timer[0] - (time.monotonic() - started)), previous_timer[1])
                diagnostic["capture_seconds"] = time.monotonic() - started
                encoded = json.dumps(diagnostic, separators=(",", ":")).encode()
                assert len(encoded) <= 256 * 1024
                (args.output / f"{prefix}.json").write_bytes(encoded + b"\n")

        try:
            initial = len(events())
            assert initial == scene_start, "startup/receiver display unexpectedly contacted a synthetic peer"
            assert not (root / "profile/config/link-previews.json").exists(), "startup wrote a default policy instead of preserving absence"
            assert image_pixels(sender_pid, args.output / "before-preview.png") < 300, "image pixel control is not discriminating"
            plain = f"off-{uuid.uuid4().hex[:8]} http://93.184.215.14/page "
            type_message(plain)
            time.sleep(1.1)  # More than the actual750ms debounce, not a success retry.
            assert len(events()) == initial, "default Off contacted a peer"
            send_and_receive(plain, False)
            mark("default_off_no_fetch_text_send")

            choose("Direct")
            body = f"card-{uuid.uuid4().hex[:8]} http://93.184.215.14/redirect "
            type_message(body)
            observation["stage"] = "ready"
            eventually(ready_card, "sender card never became ready")
            wait("Remove link preview", "push button")
            fetched = events()
            assert any(event.get("path") == "/page" for event in fetched)
            assert any(event.get("path") == "/image.png" for event in fetched)
            eventually(lambda: image_pixels(sender_pid, args.output / "sender-card.png") > 300,
                "sender safe worker image was not actually painted")
            geometry = []
            for width, zoom, steps in [(1280, 100, 0), (900, 200, 5)]:
                observation.update(stage="geometry", requested_width=width, requested_height=800, requested_zoom=zoom)
                focus_window(sender_pid)
                shell.chord("Control_L", "0")
                wait_zoom(100)
                resize_window(sender_pid, width, 800)
                for _ in range(steps):
                    shell.chord("Control_L", "equal")
                    time.sleep(.1)
                wait_zoom(zoom)
                time.sleep(.3)
                frame = window_rect(sender_pid, frame=True)
                assert frame[2:] == [width, 800], frame
                rect = window_rect(sender_pid)
                controls = {name: wait(name).extents() for name in ("Link preview", "Remove link preview", "Message", "Send")}
                for bounds in controls.values():
                    x, y, w, h = bounds
                    assert x >= 0 and y >= 0 and w > 0 and h > 0 and x + w <= rect[2] + 1 and y + h <= rect[3] + 1, (zoom, controls, rect)
                shell.screenshot(args.output / f"sender-geometry-{zoom}.png")
                geometry.append(dict(zoom=zoom, frame=frame, window=rect, controls=controls))
            shell.chord("Control_L", "0")
            wait_zoom(100)
            previous_layout = [None]
            def reset_layout_settled():
                controls = [named(name) for name in ("Link preview", "Remove link preview", "Message", "Send")]
                if any(node is None for node in controls):
                    return False
                current = [node.extents() for node in controls]
                unchanged = current == previous_layout[0]
                previous_layout[0] = current
                return unchanged
            eventually(reset_layout_settled, "sender layout did not settle after zoom reset")
            mark("native_card_controls_and_input_fit_at_narrow_real_200_percent", geometry=geometry)
            before_send = len(events())
            send_and_receive(body, True)
            time.sleep(.4)
            assert len(events()) == before_send, "receiving/rendering a carried image initiated a fetch"
            mark("direct_sender_only_embedded_image")

            # The minimum frame exercises compact navigation and the shared
            # context region while a long draft uses the editor's own scrolling.
            observation.update(stage="compact_geometry", requested_width=860,
                requested_height=520, requested_zoom=300)
            saved_pane = json.loads((root / "profile/config/settings.json").read_text()).get("sidebar_width", 248)
            compact_body = f"compact-{uuid.uuid4().hex[:8]} http://93.184.215.14/page " + "retained draft text " * 12
            type_message(compact_body)
            eventually(ready_card, "compact draft card did not reach Ready")
            assert copied_input(select_all=True) == compact_body
            shell.chord("Control_L", "Home")
            shell.press("Control_L", "Shift_L", "Right")
            selection = copied_input()
            assert 0 < len(selection) < len(compact_body) and compact_body.startswith(selection)
            resize_window(sender_pid, 860, 520)
            for _ in range(7):
                shell.chord("Control_L", "equal")
                time.sleep(.1)
            wait_zoom(300)
            assert window_rect(sender_pid, frame=True)[2:] == [860, 520]
            assert copied_input() == selection, "minimum-window layout changed the selected text"
            assert copied_input(select_all=True) == compact_body, "minimum-window layout changed the draft"
            compact_rect = window_rect(sender_pid)
            def inside(node, region):
                x, y, width, height = node.extents()
                rx, ry, rw, rh = region
                return width > 0 and height > 0 and x >= rx and y >= ry and x + width <= rx + rw + 1 and y + height <= ry + rh + 1
            timeline = wait("Messages in #Root", "list")
            for node in (wait("Message"), wait("Send"), timeline):
                eventually(lambda node=node: inside(node, (0, 0, compact_rect[2], compact_rect[3])),
                    "minimum-window editor, Send or timeline left its viewport")
            assert timeline.extents()[3] >= wait("Message").extents()[3], "timeline lost its usable text line"
            details = wait("Message details and controls", "panel")
            assert details.call("Component", "GrabFocus")[0]
            eventually(lambda: details.focused, "message details did not acquire keyboard focus")
            shell.key("Home")
            channels_top = wait("Channels", "push button").extents()
            ox, oy, _, _ = compact_rect
            dx, dy, dw, dh = details.extents()
            shell.wheel(ox + dx + dw // 2, oy + dy + dh // 2, 12)
            eventually(lambda: wait("Channels", "push button").extents() != channels_top,
                "wheel did not move compact message details")
            shell.key("Home")
            eventually(lambda: wait("Channels", "push button").extents() == channels_top,
                "keyboard Home did not restore compact message details")
            own = next(user for user in receiver.users.values() if user["name"] == "Welcome reader")
            voice_channel = own["channel"]
            voice_start = len(receiver.events)
            def reveal_channel(name):
                deadline = time.monotonic() + 10
                for attempt in range(12):
                    assert time.monotonic() < deadline, "channel wheel reveal exceeded its deadline"
                    dialog = wait("Channels and servers", "dialog")
                    tree = wait("Channels", "tree")
                    target = wait(name, "tree item")
                    row, branch, modal = target.extents(), tree.extents(), dialog.extents()
                    window = window_rect(sender_pid)
                    left = max(branch[0], modal[0], 0)
                    top = max(branch[1], modal[1], 0)
                    right = min(branch[0] + branch[2], modal[0] + modal[2], window[2])
                    bottom = min(branch[1] + branch[3], modal[1] + modal[3], window[3])
                    tree_visible = (branch[2] > 0 and branch[3] > 0
                        and left <= branch[0] and branch[0] + branch[2] <= right
                        and top <= branch[1] and branch[1] + branch[3] <= bottom)
                    if tree_visible and (row[2] > 0 and row[3] > 0
                        and left <= row[0] and row[0] + row[2] <= right
                        and top <= row[1] and row[1] + row[3] <= bottom):
                        return target
                    if tree_visible:
                        region = "channels"
                        pointer = ((left + right) / 2, (top + bottom) / 2)
                        direction = -1 if row[1] < top else 1
                    else:
                        region = "sheet"
                        # The reserved gutter reveals the complete nested viewport.
                        pointer = (min(modal[0] + modal[2], window[2]) - 6 * saved_zoom(),
                            (max(modal[1], 0) + min(modal[1] + modal[3], window[3])) / 2)
                        direction = -1 if branch[1] < max(modal[1], 0) else 1
                    trace = dict(target=name, attempt=attempt, row=row, tree=branch, modal=modal,
                        client=window, region=region, pointer=pointer, direction=direction)
                    observation.setdefault("channel_wheel_reveal", []).append(trace)
                    assert len(observation["channel_wheel_reveal"]) <= 24
                    assert max(modal[0], 0) <= pointer[0] < min(modal[0] + modal[2], window[2])
                    assert max(modal[1], 0) <= pointer[1] < min(modal[1] + modal[3], window[3])
                    shell.wheel(window[0] + round(pointer[0]), window[1] + round(pointer[1]), direction)
                    eventually(lambda: (target.extents(), tree.extents(), dialog.extents()) != (row, branch, modal),
                        "channel wheel step must move the requested region", timeout=max(.01, min(1, deadline - time.monotonic())))
                    if region == "sheet":
                        # A fresh gesture transfers routing after the 500 ms wheel latch.
                        time.sleep(.6)
                raise AssertionError("channel wheel reveal exceeded 12 steps")
            activate("Channels")
            wait("Channels and servers", "dialog")
            lobby = reveal_channel("Lobby")
            assert lobby.call("Component", "GrabFocus")[0]
            lobby.press()
            wait("Messages in #Lobby", "list")
            time.sleep(.25)
            assert receiver.users[own["session"]]["channel"] == voice_channel
            assert all(event["user"]["channel"] == voice_channel for event in list(receiver.events)[voice_start:]
                if event.get("kind") == "user" and event["user"]["session"] == own["session"])
            activate("Close channels")
            eventually(lambda: named("Channels and servers", "dialog") is None, "Close channels kept the sheet open")
            wait("Messages in #Lobby", "list")
            activate("Channels")
            wait("Channels and servers", "dialog")
            shell.key("Escape")
            eventually(lambda: named("Channels and servers", "dialog") is None, "Escape kept the sheet open")
            activate("Channels")
            root_channel = reveal_channel("Root")
            assert root_channel.call("Component", "GrabFocus")[0]
            root_channel.press()
            wait("Messages in #Root", "list")
            activate("Close channels")
            eventually(lambda: named("Channels and servers", "dialog") is None, "returning to Root kept the sheet open")
            assert copied_input(select_all=True) == compact_body, "returning to Root changed the retained draft"
            eventually(ready_card, "returning to Root did not restore the ready preview")
            timeline = wait("Messages in #Root", "list")
            timeline_identity = (timeline.destination, timeline.path)
            assert timeline.destination == application.destination
            def timeline_focused():
                current = named("Messages in #Root", "list")
                if (focused_window_pid() != sender_pid or current is None
                    or (current.destination, current.path) != timeline_identity):
                    return False
                pending = [(timeline, 0, [timeline_identity])]
                visited = set()
                while pending:
                    node, depth, ancestry = pending.pop()
                    identity = (node.destination, node.path)
                    assert identity not in visited, "timeline focus traversal repeated a node"
                    visited.add(identity)
                    assert len(visited) <= 1024 and depth <= 32, "timeline focus traversal exceeded its bounds"
                    assert node.destination == application.destination, "timeline focus left the sender application"
                    if node.focused:
                        current = named("Messages in #Root", "list")
                        if (current is None or (current.destination, current.path) != timeline_identity
                            or focused_window_pid() != sender_pid):
                            return False
                        observation["timeline_focus"] = dict(root=timeline_identity, leaf=identity,
                            ancestry=ancestry, role=node.role, name=node.name[:256])
                        return True
                    children = node.children()
                    assert len(visited) + len(pending) + len(children) <= 1024, "timeline focus traversal exceeded its node budget"
                    pending.extend((child, depth + 1, ancestry + [(child.destination, child.path)])
                        for child in children)
                return False
            assert timeline.call("Component", "GrabFocus")[0]
            eventually(timeline_focused, "compact timeline subtree did not acquire keyboard focus")
            shell.key("Home")
            shell.key("End")
            def latest():
                return next((node for node, role, label, _ in items(application)
                    if role == "list item" and body.split()[0] in label), None)
            latest_row = eventually(latest, "compact timeline lost the previously sent message")
            def latest_visible():
                _, y, width, height = latest_row.extents()
                _, top, _, viewport = timeline.extents()
                return width > 1 and height > 1 and max(y, top) < min(y + height, top + viewport)
            eventually(latest_visible, "keyboard End did not reveal the latest message")
            before_wheel = latest_row.extents()
            tx, ty, tw, th = timeline.extents()
            shell.wheel(ox + tx + tw // 2, oy + ty + th // 2, -12)
            eventually(lambda: latest_row.extents() != before_wheel, "wheel did not scroll the compact timeline")
            shell.key("End")
            eventually(latest_visible, "keyboard End did not restore the latest message after wheel scrolling")
            remove = wait("Remove link preview", "push button")
            assert remove.call("Component", "GrabFocus")[0]
            eventually(lambda: inside(remove, details.extents()), "focus did not reveal the compact Remove control")
            compact_controls = {name: wait(name).extents() for name in
                ("Message", "Send", "Messages in #Root", "Remove link preview")}
            shell.screenshot(args.output / "sender-minimum-300.png")
            activate("Remove link preview")
            wait("Link preview removed")
            assert copied_input(select_all=True) == compact_body
            send_and_receive(compact_body, False)
            shell.chord("Control_L", "0")
            wait_zoom(100)
            resize_window(sender_pid, 1280, 800)
            eventually(lambda: named("Channels", "tree") is not None and named("Channels", "push button") is None,
                "wide window did not restore inline navigation")
            assert json.loads((root / "profile/config/settings.json").read_text()).get("sidebar_width", 248) == saved_pane
            mark("minimum_real_300_percent_retains_draft_selection_navigation_and_timeline",
                frame=[860, 520], zoom=300, controls=compact_controls,
                selection=selection, saved_sidebar_width=saved_pane, voice_channel=voice_channel)

            # Explicit removal of a ready card suppresses attachment while the
            # entire original text still goes in one ordinary message.
            removed = f"removed-{uuid.uuid4().hex[:8]} http://93.184.215.14/page "
            type_message(removed)
            eventually(ready_card, "ready-card removal case never reached Ready")
            wait("Remove link preview", "push button")
            activate("Remove link preview")
            wait("Link preview removed")
            send_and_receive(removed, False)
            mark("removed_card_text_only")

            # Fresh URL and a real response gate exercise late-result suppression.
            control("arm_http", 39212)
            baseline = sum(event.get("kind") == "http_gate_entered" for event in events())
            delayed = f"late-{uuid.uuid4().hex[:8]} http://93.184.215.14/page?late=1 "
            type_message(delayed)
            eventually(lambda: sum(event.get("kind") == "http_gate_entered" for event in events()) > baseline,
                "controlled page request did not reach response gate")
            activate("Remove link preview")
            wait("Link preview removed")
            control("release_http", 39212)
            time.sleep(1.1)
            wait("Link preview removed")
            send_and_receive(delayed, False)
            mark("late_response_cannot_undo_remove")

            # Explicit fixture operator services, not ambient proxy settings. The
            # real UI save reloads this managed policy before applying the choice.
            managed_path = root / "profile/config/managed-link-previews.json"
            managed_bytes = json.dumps({
                "version": 1, "mode": "proxy_required", "proxy": "socks5://127.0.0.1:1080",
                "dns": {"url": "https://resolver.preview.example/dns-query", "bootstrap": ["93.184.215.53"]},
            }).encode()
            managed_path.write_bytes(managed_bytes)
            choose("Proxy required")
            before = events()
            proxy_text = f"proxy-{uuid.uuid4().hex[:8]} http://page.preview.example/page "
            type_message(proxy_text)
            wait("Link preview unavailable")
            after = events()[len(before):]
            assert any(event.get("kind") == "socks" and event.get("destination") == "93.184.215.53" for event in after), after
            assert not any(event.get("kind") in ("dns", "doh") or event.get("destination") == "93.184.215.14" for event in after), after
            send_and_receive(proxy_text, False)
            mark("production_doh_untrusted_ca_refused_without_direct_or_dns_fallback")

            # The saved user mode remains visible; repeated choices are real UI
            # actions, while active policy only resumes after its serialized save.
            choose("Off")
            sidecar = json.loads((root / "profile/config/link-previews.json").read_text())
            assert sidecar["mode"] == "off"
            assert sidecar.get("proxy") is None and sidecar.get("dns") is None, "managed services were copied into user authority"
            assert managed_path.read_bytes() == managed_bytes, "UI save changed the operator's route"
            events()  # No synthetic peer assertion may hide at the scenario's end.
            mark("policy_persisted_without_copying_or_losing_service_settings")
            return {"server": args.server, "cases": cases, "modern_typed_preview": False,
                "production_tls_bypass": False, "host_devices": False}
        except BaseException as failure:
            try:
                capture_failure(failure)
            except BaseException as capture_error:
                # Diagnostics cannot replace the original assertion/exception.
                print(f"bounded scene capture failed: {type(capture_error).__name__}", flush=True)
            raise

    original = scenario.__dict__.copy()
    scenario.WELCOME_EXTRA = ""
    scenario.COMMENT = "Owned link preview fixture"
    scenario.seed_description = lambda _path: None
    scenario.configure_handler = configure
    scenario.run_actions = actions
    welcome_html.tempfile = types.SimpleNamespace(TemporaryDirectory=RetainedDirectory)
    if args.server == "nuschld":
        welcome_html.Murmur = Compatible
    args.document_links, args.expect_markers = True, False
    try:
        welcome_html.run(args)
    except BaseException:
        # Root retains the owned profile/keys privately, never in a public log.
        (args.output / "failed.json").write_text(json.dumps({"cases_completed": len(cases)}) + "\n")
        raise
    finally:
        scenario.__dict__.update(original)
        for child in extra_children:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(8)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    server = commands.add_parser("peers")
    server.add_argument("--directory", type=Path, required=True)
    native = commands.add_parser("native-peers")
    native.add_argument("--directory", type=Path, required=True)
    gui = commands.add_parser("desktop")
    for name in ("binaries", "murmur", "at-spi", "gnome-shell", "pulse-bin", "output"):
        gui.add_argument("--" + name, type=Path, required=True)
    gui.add_argument("--server", choices=("stock", "nuschld"), required=True)
    gui.add_argument("--source-identity", required=True)
    args = parser.parse_args()
    {"peers": peers, "native-peers": native_peers, "desktop": desktop}[args.command](args)


if __name__ == "__main__":
    main()
