#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual native scrollbar/history interactions in owned GNOME/Pulse/Murmur.
No host profile, clipboard, microphone, speakers or public server is used.
The private storage child is briefly stopped to observe the before/after anchor
of an actual asynchronous older page; only descendants of this fixture are touched.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import uuid

from accessibility import items, start_registry
from gnome_session import GnomeSession
from local_murmur import Murmur, eventually, free_port
from mumble_client import Client, certificate_pin
from slider_scenes import Scenes

PATTERN = re.compile(r'History Writer: Scroll history (\d+)\b')
COLORS = ((53, 62, 78), (148, 159, 178), (154, 164, 183))


class ScrollbarScenes(Scenes):
    def __init__(self, args, root):
        super().__init__(args, root)
        self.proof = {'cases': [], 'package': str(args.binaries.parent),
                      'desktop_sha256': hashlib.sha256((args.binaries.parent / 'libexec/nuschl/nuschl').read_bytes()).hexdigest(),
                      'fixture_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      'private_audio': 'owned Pulse null devices only',
                      'host_devices_or_clipboard': False}
        self.stopped = []

    def screen(self, x, y):
        # AccessKit window coordinates include GPUI's client-side shadow. The
        # compositor frame excludes it; thin scrollbar hitboxes need the buffer.
        rect = json.loads(self.shell.eval("JSON.stringify((()=>{const w=global.get_window_actors()"
            ".map(a=>a.meta_window).find(w=>(w.get_title()||'').startsWith('Nuschl'));"
            "const r=w.get_buffer_rect();return{x:r.x,y:r.y,width:r.width,height:r.height};})())"))
        return rect['x'] + x, rect['y'] + y

    def timeline(self):
        return self.wait_for('list', 'Messages in #Development', 'missing timeline')

    def loaded(self):
        result = {}
        for node in self.timeline().children():
            if match := PATTERN.search(node.name):
                result[int(match[1])] = node
        return result

    def visible(self):
        x, top, width, height = self.timeline().extents()
        result = {}
        for number, node in self.loaded().items():
            left, y, w, h = node.extents()
            if h > 2 and top < y + h and y < top + height:
                result[number] = [left, y, w, h]
        assert result, 'no physically laid-out timeline message'
        return result

    def gutter(self):
        node = self.wait_for(None, 'Chat history scrollbar', 'missing scrollbar area')
        x, y, width, height = node.extents()
        tx, ty, tw, th = self.timeline().extents()
        assert width >= 8 and height > 100, [x, y, width, height]
        assert tx <= x and x + width <= tx + tw + 2 and abs(y - ty) <= 2 and abs(height - th) <= 2, (
            [x, y, width, height], [tx, ty, tw, th])
        return x, y, width, height

    def thumb(self, label):
        x, y, width, height = self.gutter()
        self.shell.move(*self.screen(x - 30, y + height / 2))
        time.sleep(.25)
        path = self.shell.screenshot(self.out / (label + '-gutter.png'),
                                     crop=(*self.screen(x, y), width, height))
        pixels = subprocess.check_output(['magick', str(path), '-alpha', 'off', '-depth', '8', 'rgb:-'])
        assert len(pixels) == width * height * 3
        rows = []
        for row in range(height):
            hits = 0
            for column in range(width):
                rgb = pixels[(row * width + column) * 3:(row * width + column + 1) * 3]
                if any(max(abs(a - b) for a, b in zip(rgb, color)) <= 2 for color in COLORS):
                    hits += 1
            rows.append(hits >= 2)
        runs, begin = [], None
        for row, present in enumerate(rows + [False]):
            if present and begin is None:
                begin = row
            elif not present and begin is not None:
                runs.append((begin, row)); begin = None
        assert runs, 'scrollbar thumb is not visibly painted'
        start, end = max(runs, key=lambda run: run[1] - run[0])
        assert end - start >= 30, (start, end, width, height)
        return self.screen(x + width / 2, y + (start + end) / 2), [x, y + start, width, end - start]

    def private_storage(self):
        pending, graph = [self.app.pid], {}
        while pending:
            pid = pending.pop()
            if pid in graph:
                continue
            assert len(graph) < 512, 'unexpected owned process fanout'
            try:
                children = set()
                for path in Path(f'/proc/{pid}/task').glob('*/children'):
                    children.update(map(int, path.read_text().split()))
                command = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
                graph[pid] = {'children': sorted(children),
                              'argv': [os.fsdecode(arg) for arg in command if arg]}
                pending.extend(children)
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                pass
        # bwrap renames its executable mount to /worker. Identify its actual
        # role from our own launcher's fixed --ro-bind input, then take its leaf.
        roots = [pid for pid, data in graph.items() if any(
            Path(arg).name == 'nuschl-storage-worker' for arg in data['argv'])]
        leaves = set()
        for root in roots:
            pending = [root]
            while pending:
                pid = pending.pop()
                children = graph.get(pid, {}).get('children', [])
                if children:
                    pending.extend(children)
                else:
                    leaves.add(pid)
        (self.out / 'owned-processes.json').write_text(json.dumps(graph, indent=2) + '\n')
        assert len(leaves) == 1, f'expected one owned storage leaf: roots={roots}, leaves={leaves}'
        return leaves.pop()

    def release_storage(self):
        for pid in self.stopped:
            os.kill(pid, signal.SIGCONT)
        self.stopped.clear()

    def latest(self, number):
        eventually(lambda: number in self.visible(), f'latest message {number} is not visible', timeout=30)

    def channel(self, name):
        self.wait_for('tree item', name, 'missing channel ' + name).press()
        time.sleep(.5)

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, 1440, 1000,
                                  log=(self.out / 'gnome.log').open('w'))
        self.shell.start()
        self.registry = start_registry(self.args.at_spi, self.shell.app_environment(), self.spawn)
        self.start_pulse()
        port = free_port()
        server_root = self.root / 'murmur'
        server_root.mkdir()
        server = Murmur(self.args.murmur, server_root, port)
        server.ini.write_text(server.ini.read_text() + 'messagelimit=1000\nmessageburst=1000\n')
        server.seed(); server.start()
        peer = None
        try:
            pin = certificate_pin(self.args.binaries, '127.0.0.1', port, self.root / 'probe')
            profile = self.root / 'profile'
            (profile / 'config').mkdir(parents=True)
            (profile / 'config/servers.json').write_text(json.dumps([dict(
                id=str(uuid.uuid4()), name='Scrollbar server', host='127.0.0.1', port=port,
                username='Reader', certificate_sha256=pin)]))
            (profile / 'config/settings.json').write_text(json.dumps({'appearance': {'theme': 'dark'}}))
            peer = Client(self.args.binaries, '127.0.0.1', port, pin, 'History Writer', self.root / 'peer',
                          extra=('--message-limit', '1000', '--message-burst', '1000'))
            peer.send(kind='join', channel=2)
            self.launch(profile, f'mumble://Reader@127.0.0.1:{port}/Lobby/Development')
            peer.wait(lambda e: e['kind'] == 'user' and e['user']['name'] == 'Reader'
                      and e['user']['channel'] == 2, timeout=40)
            for number in range(1, 161):
                peer.send(kind='text', channel=2, body=f'Scroll history {number:03}' +
                          ('\nA second line keeps row heights varied.' if number % 5 == 0 else ''))
                time.sleep(.025)
            self.latest(160)
            time.sleep(2)
            # A different channel retains its in-memory timeline; a real app
            # restart proves paging from durable legacy history instead.
            self.quit()
            time.sleep(.5)
            self.launch(profile, f'mumble://Reader@127.0.0.1:{port}/Lobby/Development')
            self.latest(160)
            initial = sorted(self.loaded())
            assert len(initial) < 160 and initial[-1] == 160, initial
            time.sleep(3)  # prove Always remains painted after the default idle hold
            self.proof['idle_visibility_wait_seconds'] = 3
            self.shot('01-latest')
            start, _ = self.thumb('01-latest')
            pid = self.private_storage()
            os.kill(pid, signal.SIGSTOP); self.stopped.append(pid)
            eventually(lambda: re.search(r'^State:\s+[Tt]\b', Path(f'/proc/{pid}/status').read_text(), re.M),
                       'owned storage worker did not enter stopped state')
            x, y, w, h = self.gutter()
            self.shell.drag(start, self.screen(x + w/2, y + 2), steps=18)
            time.sleep(.4)
            assert sorted(self.loaded()) == initial, 'history advanced despite stopped storage child'
            before = self.visible()
            # The keeper anchors the FIRST visible stored message. Its row
            # can lose an author header when older same-author rows arrive;
            # lower rows legitimately reflow while this row's top stays fixed.
            anchor = min(before)
            anchor_y = before[anchor][1]
            self.shot('02-page-pending')
            self.release_storage()
            eventually(lambda: min(self.loaded()) < initial[0], 'older page never prepended', timeout=25)
            time.sleep(1)
            after = self.visible()
            assert anchor in after and abs(after[anchor][1] - anchor_y) <= 2, (anchor, before, after)
            self.proof['cases'].append({'older_prepend_anchor': anchor, 'before_y': anchor_y,
                                       'after_y': after[anchor][1], 'row_height_before': before[anchor][3],
                                       'row_height_after': after[anchor][3], 'loaded_before': initial,
                                       'loaded_after': sorted(self.loaded())})
            self.shot('03-prepended-anchor')
            # Let an actual wheel animation begin, then grab the thumb before
            # its150ms easing ends. Holding without moving must stay still.
            x, y, w, h = self.gutter()
            self.shell.wheel(*self.screen(x - 100, y + h/2), 3)
            start, _ = self.thumb('03-before-grab')
            # wheel() and a second pointer press in one compositor call, avoiding
            # bus/screenshot delay that could accidentally wait out the animation.
            self.shell.move(*start)
            content_x, content_y = self.screen(x - 100, y + h/2)
            self.shell.eval("const p=global._nuschlPointer,C=imports.gi.Clutter,t=imports.gi.GLib.get_monotonic_time();"
                            f"p.notify_absolute_motion(t,{content_x},{content_y});"
                            "p.notify_discrete_scroll(t+1000,C.ScrollDirection.DOWN,C.ScrollSource.WHEEL);"
                            f"p.notify_absolute_motion(t+2000,{start[0]},{start[1]});"
                            "p.notify_button(t+3000,1,C.ButtonState.PRESSED); 'ok'")
            time.sleep(.05)
            held = self.visible()
            key = sorted(held)[len(held)//2]
            time.sleep(.35)
            settled = self.visible()
            assert key in settled and abs(held[key][1] - settled[key][1]) <= 2, (held, settled)
            self.release()
            self.proof['cases'].append({'wheel_then_stationary_grab': key,
                                       'before_y': held[key][1], 'after_y': settled[key][1]})
            # Move down and up with the physical thumb, then preserve unread
            # position while a new real message arrives and use its jump pill.
            start, _ = self.thumb('04-thumb')
            self.shell.drag(start, self.screen(x + w/2, y + h - 2), steps=18)
            self.latest(160)
            start, _ = self.thumb('04-bottom')
            self.shell.drag(start, self.screen(x + w/2, y + h*.25), steps=18)
            time.sleep(.8)
            prior = self.visible()
            anchor = sorted(prior)[len(prior)//2]
            peer.send(kind='text', channel=2, body='Scroll history 161 live unread')
            time.sleep(1)
            after = self.visible()
            assert 161 not in after and anchor in after and abs(prior[anchor][1] - after[anchor][1]) <= 2
            pill = eventually(lambda: next((n for n, r, name, _ in items(self.nuschl)
                                            if r == 'push button' and 'Jump to latest' in name), None),
                              'no unread jump-to-latest pill')
            pill.press(); self.latest(161)
            self.shot('05-jump-to-latest')
            # Keyboard navigation remains in the existing timeline, independent
            # of the mouse-only overlay; real Ctrl++ and narrow panes keep it inside.
            self.timeline().call('Component', 'GrabFocus')
            self.shell.key('Home'); time.sleep(.6)
            self.shell.key('End'); self.latest(161)
            for width, zoom_steps in ((900, 0), (900, 5)):
                self.shell.place('Nuschl', 40, 40, width, 800)
                self.shell.chord('Control_L', '0')
                if zoom_steps:
                    for _ in range(zoom_steps):
                        self.shell.chord('Control_L', 'equal')
                        time.sleep(.1)
                expected_zoom = 2.0 if zoom_steps else 1.0
                eventually(lambda: abs(json.loads((profile / 'config/settings.json').read_text())
                    .get('appearance', {}).get('zoom', 1.0) - expected_zoom) < .001,
                    'actual Ctrl++ zoom did not persist')
                time.sleep(.8)
                bounds = self.gutter()
                thumb, painted = self.thumb(f'06-{width}-{zoom_steps}')
                self.shot(f'06-{width}-{zoom_steps}')
                self.proof['cases'].append({'window_width': width, 'actual_zoom': expected_zoom, 'zoom_key_steps': zoom_steps,
                                           'gutter_bounds': bounds, 'painted_thumb': painted})
            assert self.recording() == [], 'fixture unexpectedly opened a capture stream'
            self.proof['no_private_capture_streams'] = True
            self.proof['passed'] = True
        finally:
            self.release_storage()
            if peer: peer.close()
            server.stop()
            (self.out / 'proof.json').write_text(json.dumps(self.proof, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binaries', type=Path)
    parser.add_argument('--murmur', type=Path, required=True)
    parser.add_argument('--gnome-shell', type=Path, required=True)
    parser.add_argument('--at-spi', type=Path, required=True)
    parser.add_argument('--pulse-bin', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    for name in ('tmp', 'home', 'fixture'):
        (args.output / name).mkdir()
    os.environ.update(TMPDIR=str(args.output / 'tmp'), NUSCHL_WL_TMP=str(args.output / 'tmp'),
                      HOME=str(args.output / 'home'), NUSCHL_AUDIO_BACKEND='pulse',
                      PULSE_SERVER='unix:/nuschl-private-no-audio', PIPEWIRE_REMOTE='nuschl-private-no-pipewire')
    scenes = ScrollbarScenes(args, args.output / 'fixture')
    try:
        scenes.run()
    finally:
        scenes.close()
    print('Native history scrollbar, dragging, anchor, wheel/keys, unread jump and zoom checks passed.')


if __name__ == '__main__':
    main()
