#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Private musicbot artwork fixture: real worker/HTTP/Canvas and cancellation.

Only owned Murmur/Subsonic endpoints, generated PNGs and dedicated browser state.
No host microphone, camera, clipboard, profile or external server is used.
"""
import argparse
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import threading
import time
import urllib.parse
import zlib

from fake_subsonic import ALBUM, Library
from local_murmur import Murmur, free_port
from mumble_client import Client, certificate_pin
from musicbot import API_KEY, PASSWORD, BotProcess, api, link_from, session_of, write_config
from web_audio import prepare_browser

ROOT = Path(__file__).resolve().parents[1]


def solid_png(color):
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    row = b'\0' + bytes(color) * 400
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 400, 400, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(row * 400)) + chunk(b'IEND', b'')


class Colors(Library):
    def handle_request(self):
        url = urllib.parse.urlparse(self.path)
        parts = url.path.strip('/').split('/')
        query = urllib.parse.parse_qs(url.query)
        if len(parts) == 3 and parts[0] in ('plain', 'open') and parts[2] in ('getCoverArt', 'getAlbumList2'):
            library, _, endpoint = parts
            if self.authenticated(library, query).startswith('wrong'):
                self.failure(40, 'fixture credentials rejected')
                return
            if endpoint == 'getAlbumList2':
                self.reply({'albumList2': {'album': [{**ALBUM, 'id': f'a{i}', 'name': f'Waves {i}', 'coverArt': f'cover{i}'} for i in range(48)] if query.get('offset') == ['0'] else []}})
                return
            if query.get('id') == ['missing']:
                self.failure(70, 'no such cover')
                return
            body = solid_png([200, 80, 40] if library == 'plain' else [40, 80, 200])
            self.send_response(200)
            self.send_header('Content-Type', 'image/png')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().handle_request()

    do_GET = handle_request
    do_POST = handle_request


class NoForwardProxy(http.server.BaseHTTPRequestHandler):
    blocked = 0
    def log_message(self, *_):
        pass
    def do_CONNECT(self):
        type(self).blocked += 1
        self.send_error(403)
    def do_GET(self):
        type(self).blocked += 1
        self.send_error(403)


def descendants(pid):
    found, pending = set(), [pid]
    while pending:
        parent = pending.pop()
        for thread in (Path('/proc') / str(parent) / 'task').glob('*'):
            try:
                children = [int(value) for value in (thread / 'children').read_text().split()]
            except OSError:
                continue
            for child in children:
                if child not in found:
                    found.add(child)
                    pending.append(child)
    return found


def workers(pid):
    found = []
    for child in descendants(pid):
        try:
            command = (Path('/proc') / str(child) / 'cmdline').read_bytes().split(b'\0')
            if command[0] == b'/worker':
                found.append(child)
        except OSError:
            pass
    return found


def wait(check, description, timeout=12):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = check()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(description)


def cancelled_http(link, bot):
    url = urllib.parse.urlparse(link)
    clients = []
    try:
        for index in range(2):
            client = http.client.HTTPConnection(url.hostname, url.port, timeout=2)
            client.request('GET', url.path + f'art/0/cover{index}?size=300')
            clients.append(client)
        try:
            pending = wait(lambda: workers(bot.process.pid) if len(workers(bot.process.pid)) == 2 else None,
                           'two actual stalled artwork children', timeout=4)
        except AssertionError:
            for client in clients:
                response = client.getresponse()
                print('unexpected early artwork response', response.status, response.read(4096))
            raise
        for client in clients:
            client.close()
        check = http.client.HTTPConnection(url.hostname, url.port, timeout=2)
        try:
            started = time.monotonic()
            check.request('GET', url.path + 'art/0/missing')
            response = check.getresponse()
            assert response.status == 503, (response.status, response.read())
            assert time.monotonic() - started < 1
            response.read()
        finally:
            check.close()
        assert set(pending) <= set(workers(bot.process.pid)), 'cancellation prematurely retired/replaced workers'
        wait(lambda: not any((Path('/proc') / str(pid)).exists() for pid in pending), 'actual worker exit', timeout=10)
        # A failed-image request proves real admission has returned, without
        # intentionally creating another stalled decoder process.
        def available():
            client = http.client.HTTPConnection(url.hostname, url.port, timeout=2)
            try:
                client.request('GET', url.path + 'art/0/missing')
                response = client.getresponse()
                response.read()
                if response.status == 503:
                    return False
                assert response.status == 404, response.status
                return True
            finally:
                client.close()
        wait(available, 'released cover capacity')
        assert not workers(bot.process.pid)
        return {'cancelled_http_requests': 2, 'real_children_before_cancel': 2,
                'overload_while_unacknowledged': 503, 'all_worker_pids_exited': True,
                'admission_reusable_after_exit': True}
    finally:
        for client in clients:
            client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--cli', type=Path, required=True)
    parser.add_argument('--murmur', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cancel-probe', type=Path)
    parser.add_argument('--chromium', type=Path)
    parser.add_argument('--font-dir', type=Path)
    parser.add_argument('--playwright', type=Path)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    wrapper = (args.package / 'bin/nuschl-musicbot').read_text()
    assets = Path(re.search(r"/nix/store/[a-z0-9]{32}-nuschl-musicbot-web-[^'\"\s}]+", wrapper).group())
    binaries = args.output / 'bundle/bin'
    binaries.mkdir(parents=True, exist_ok=True)
    (binaries / 'nuschl-cli').symlink_to(args.cli)
    if args.cancel_probe:
        # A dedicated trusted fixture bundle substitutes only the worker for a
        # bounded faulting worker. The production bot ELF is unchanged.
        shutil.copyfile(args.package / 'bin/.nuschl-musicbot-wrapped', binaries / 'nuschl-musicbot')
        (binaries / 'nuschl-musicbot').chmod(0o700)
        worker = args.output / 'bundle/libexec/nuschl-musicbot/nuschl-content-worker'
        worker.parent.mkdir(parents=True)
        shutil.copyfile(args.cancel_probe, worker)
        worker.chmod(0o700)
    else:
        (binaries / 'nuschl-musicbot').symlink_to(args.package / 'bin/nuschl-musicbot')
    server_root = args.output / 'murmur'
    server_root.mkdir()
    murmur = Murmur(args.murmur, server_root, free_port())
    library = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Colors)
    proxy = http.server.ThreadingHTTPServer(('127.0.0.1', 0), NoForwardProxy)
    threads = [threading.Thread(target=server.serve_forever) for server in [library, proxy]]
    bot = listener = None
    proof = {'package': str(args.package), 'artwork_helper_sha256': hashlib.sha256((args.package/'libexec/nuschl-musicbot/nuschl-content-worker').read_bytes()).hexdigest()}
    try:
        murmur.seed(); murmur.start()
        for thread in threads:
            thread.start()
        pin = certificate_pin(binaries, '127.0.0.1', murmur.port, args.output / 'pin')
        (args.output / 'password').write_text(PASSWORD + '\n')
        (args.output / 'key').write_text(API_KEY + '\n')
        for secret in ['password', 'key']:
            (args.output / secret).chmod(0o600)
        config = args.output / 'bot.toml'
        write_config(config, args.output, f'http://127.0.0.1:{library.server_port}', free_port(), murmur.port, pin)
        args.binaries, args.web_assets = binaries, assets
        bot = BotProcess(args, config, args.output)
        listener = Client(binaries, '127.0.0.1', murmur.port, pin, 'art-fixture', args.output / 'client')
        link = link_from(listener, session_of(listener, 'DJ'))
        assert api(link)['libraries'] == ['Plain', 'Open']
        if args.cancel_probe:
            proof['cancellation'] = cancelled_http(link, bot)
        else:
            if not all([args.chromium, args.font_dir, args.playwright]):
                parser.error('browser mode requires chromium/font-dir/playwright')
            browser = prepare_browser(args.output / 'browser', args.chromium, shutil.which('bwrap'), args.font_dir)
            command = ['node', str(ROOT/'tests/musicbot_artwork.browser.mjs'), link,
                       browser['wrapper'], str(args.playwright), str(args.output), str(proxy.server_port)]
            env = dict(os.environ, TMPDIR=browser['browser_tmpdir'])
            subprocess.run(command, env=env, check=True, timeout=180)
            proof['browser'] = json.loads((args.output / 'browser-proof.json').read_text())
            proof['blocked_background_proxy_requests'] = NoForwardProxy.blocked
    finally:
        if listener:
            listener.close()
        if bot:
            try:
                bot.stop()
            finally:
                (args.output / 'bot.log').write_text(''.join(bot.log))
        murmur.stop()
        for server, thread in zip([library, proxy], threads):
            if thread.ident:
                server.shutdown()
            server.server_close()
        for thread in threads:
            if thread.ident:
                thread.join(timeout=5)
                assert not thread.is_alive()
    proof['owned_services_stopped'] = True
    (args.output / 'proof.json').write_text(json.dumps(proof, indent=2) + '\n')
    print(json.dumps(proof))


if __name__ == '__main__':
    main()
