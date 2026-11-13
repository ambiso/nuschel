#!/usr/bin/env python3
"""Exercise JSON/SSE music control and stock Mumble audio end to end.

Usage: with_local_murmur.py <mumble-server> tests/musicbot.py <binaries> PORT
           --espeak <espeak-ng> --opus-library <libopus.so> [--web-assets <dist>]
       musicbot.py <binaries> <port> --service   (an already running VM service)

Two independent viewer tokens control shared queues. Plain Subsonic and
OpenSubsonic browsing, packet fan-out, speech-only VAD, seeking, and graceful
deployment with reordered configuration are checked through observable APIs
and the audio received by real Mumble listeners.
"""
import argparse
import base64
from collections import Counter, deque
import html
import ctypes
import http.client
import http.server
import json
import math
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import wave

sys.path.insert(0, str(Path(__file__).resolve().parent))
from fake_subsonic import API_KEY, COVER, COVER_PNG, PASSWORD, Library  # noqa: E402
from mumble_client import Client, ReferenceOpus, certificate_pin  # noqa: E402

MUSIC, LOBBY = 6, 1  # local_murmur.py's channels


HTTP_TIMEOUT = 20
EVENT_TIMEOUT = 15


def api(link, endpoint="state", **query):
    url = link + "api/" + endpoint
    if query:
        url += "?" + urllib.parse.urlencode(query)
    with urllib.request.urlopen(url, timeout=HTTP_TIMEOUT) as response:
        assert response.status == 200, response.status
        assert response.headers.get_content_type() == "application/json", dict(response.headers)
        return json.load(response)


def command(link, action, player=0, **fields):
    request = urllib.request.Request(
        link + "api/command", method="POST",
        data=json.dumps({"command": action, "player": player, **fields}).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT) as response:
        assert response.status == 204 and response.read() == b"", response.status


def eventually(observe, predicate, description, timeout=EVENT_TIMEOUT):
    """Poll only asynchronous state transitions; HTTP/protocol errors escape."""
    deadline = time.monotonic() + timeout
    value = None
    while time.monotonic() < deadline:
        value = observe()
        if predicate(value):
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {description}: {value}")


class Events:
    """Parse real SSE frames in a reader thread, retaining snapshots for assertions."""

    def __init__(self, link):
        url = urllib.parse.urlsplit(link + "api/events")
        self.connection = http.client.HTTPConnection(url.hostname, url.port, timeout=HTTP_TIMEOUT)
        try:
            self.connection.request("GET", url.path, headers={"Accept": "text/event-stream"})
            self.response = self.connection.getresponse()
            assert self.response.status == 200, self.response.status
            assert self.response.headers.get_content_type() == "text/event-stream", dict(self.response.headers)
            self.socket = self.connection.sock or self.response.fp.raw._sock
        except Exception:
            self.connection.close()
            raise
        self.condition = threading.Condition()
        self.snapshots = []
        self.error = None
        self.ended = False
        self.closing = False
        self.thread = threading.Thread(target=self.read, name="musicbot-sse")
        self.thread.start()

    def read(self):
        try:
            event, data = "", []
            while True:
                line = self.response.readline()
                if not line:
                    break
                line = line.decode("utf-8").rstrip("\r\n")
                if not line:
                    if data:
                        assert event == "state", event
                        snapshot = json.loads("\n".join(data))
                        with self.condition:
                            self.snapshots.append(snapshot)
                            self.condition.notify_all()
                    event, data = "", []
                elif line.startswith(":"):
                    continue
                else:
                    field, separator, value = line.partition(":")
                    if separator and value.startswith(" "):
                        value = value[1:]
                    if field == "event":
                        event = value
                    elif field == "data":
                        data.append(value)
        except Exception as error:
            if not self.closing:
                self.error = error
        finally:
            with self.condition:
                self.ended = True
                self.condition.notify_all()

    def mark(self):
        with self.condition:
            return len(self.snapshots)

    def wait(self, predicate, after=0, timeout=EVENT_TIMEOUT):
        deadline = time.monotonic() + timeout
        with self.condition:
            while True:
                if self.error:
                    raise AssertionError(f"SSE reader failed: {self.error}") from self.error
                for snapshot in self.snapshots[after:]:
                    if predicate(snapshot):
                        return snapshot
                assert not self.ended, "SSE ended before the expected state"
                remaining = deadline - time.monotonic()
                assert remaining > 0, f"SSE state timed out: {self.snapshots[-1:]}"
                self.condition.wait(remaining)

    def close(self):
        self.closing = True
        if self.socket:
            try:
                self.socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.thread.join(timeout=HTTP_TIMEOUT + 1)
        assert not self.thread.is_alive(), "SSE reader did not stop"
        self.response.close()
        self.connection.close()


def broadcast(watchers, link, action, predicate, player=0, **fields):
    marks = [watcher.mark() for watcher in watchers]
    command(link, action, player, **fields)
    states = [watcher.wait(predicate, after=mark) for watcher, mark in zip(watchers, marks)]
    # User identity is deliberately token-specific; the shared paused queue is not.
    assert states[0]["players"][player] == states[1]["players"][player], states
    return states[0]


def browse(link):
    for library in (0, 1):
        for listing in ("newest", "recent", "frequent", "random", "alphabeticalByName", "starred"):
            albums = api(link, "albums", library=library, list=listing, page=0)
            assert albums["albums"][0]["name"] == "Waves" and not albums["more"], albums
            assert api(link, "albums", library=library, list=listing, page=1)["albums"] == []
        artists = api(link, "artists", library=library)
        assert artists["groups"][0]["artists"][0]["name"] == "Oscillators", artists
        artist = api(link, "artist", library=library, id="ar1")
        assert artist["artist"]["id"] == "ar1" and artist["albums"][0]["name"] == "Waves", artist
        album = api(link, "album", library=library, id="a1")
        assert album["album"]["year"] == 2026, album
        assert [song["id"] for song in album["songs"]] == ["s1", "s2"], album
        assert all(song["library"] == library and song["duration"] == 3 for song in album["songs"]), album
        assert api(link, "playlists", library=library)["playlists"][0]["name"] == "Backwards"
        playlist = api(link, "playlist", library=library, id="p1")
        assert [song["id"] for song in playlist["songs"]] == ["s2", "s1"], playlist
        cover = "art/" + str(library) + "/" + urllib.parse.quote(album["album"]["cover_art"], safe="")
        with urllib.request.urlopen(link + cover + "?size=300", timeout=HTTP_TIMEOUT) as response:
            pixels = json.load(response)
            assert set(pixels) == {"width", "height", "rgba"}, pixels
            assert pixels["width"] == pixels["height"] == 1, pixels
            assert base64.b64decode(pixels["rgba"], validate=True) == bytes([200, 80, 40, 255]), pixels
            assert response.headers.get_content_type() == "application/json", dict(response.headers)
            assert response.headers["Cache-Control"] == "private, no-store", dict(response.headers)
        # The same cover again comes from the bot's cache. With the state's art
        # key the browser may keep it; a key for another library is refused.
        asked = [seen for seen in Library.seen if seen[1] == "getCoverArt"]
        key = api(link)["art_keys"][library]
        with urllib.request.urlopen(link + cover + f"?size=300&v={key}", timeout=HTTP_TIMEOUT) as response:
            assert response.headers["Cache-Control"] == "private, max-age=86400", dict(response.headers)
            assert json.load(response) == pixels
        assert [seen for seen in Library.seen if seen[1] == "getCoverArt"] == asked, "the cover was fetched again"
        expect_error(link + cover + "?size=300&v=0000000000000000", 404)
        expect_error(link + f"art/{library}/missing", 404)
    # Only Open (index 1) advertises sonicSimilarity; Plain refuses before asking.
    expect_error(link + "api/similar?library=0&id=s1", 502, json_error=True)
    expect_error(link + "api/path?library=0&from=s1&to=s2", 502, json_error=True)
    similar = api(link, "similar", library=1, id="s1")
    assert similar["song"]["id"] == "s1" and similar["song"]["library"] == 1, similar
    assert [(found["track"]["id"], found["track"]["library"], found["similarity"])
            for found in similar["matches"]] == [("s2", 1, 0.8)], similar
    path = api(link, "path", library=1, **{"from": "s1", "to": "s2"})
    assert [(found["track"]["id"], found["similarity"]) for found in path["matches"]] == [("s1", 1.0), ("s2", 0.5)], path
    assert api(link)["sonic"] == [False, True]
    results = api(link, "search", q="sine")["results"]
    assert [result["name"] for result in results] == ["Plain", "Open"], results
    for result in results:
        assert result["error"] is None, result
        assert [song["title"] for song in result["songs"]] == ["Sine in A", "Sine in E"], result
        assert result["albums"][0]["name"] == "Waves", result
        assert result["artists"][0]["name"] == "Oscillators", result


def expect_error(url, status, data=None, headers=None, json_error=False):
    try:
        urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers or {}), timeout=HTTP_TIMEOUT)
    except urllib.error.HTTPError as error:
        assert error.code == status, (error.code, error.read())
        if json_error:
            assert isinstance(json.load(error)["error"], str)
        return
    raise AssertionError(f"{url} accepted an invalid request")


SILENT_OPUS = [0xf8, 0xff, 0xfe]


def speech_packets(espeak, opus_library, directory):
    """A spoken sentence (espeak-ng) as 20 ms Opus packets at 48 kHz."""
    path = directory / "speech.wav"
    subprocess.run([str(espeak), "-s", "150", "-w", str(path),
                    "Hello everyone, can you hear me? The music is a little loud while we talk."],
                   check=True, capture_output=True, timeout=HTTP_TIMEOUT)
    with wave.open(str(path)) as recording:
        assert recording.getsampwidth() == 2 and recording.getnchannels() == 1
        rate = recording.getframerate()
        data = recording.readframes(recording.getnframes())
    source = [value / 32768 for value in struct.unpack(f"<{len(data) // 2}h", data)]
    # Linear interpolation to 48 kHz is plenty for voice activity detection.
    samples = []
    for index in range(len(source) * 48000 // rate):
        position = index * rate / 48000
        left = int(position)
        right = min(left + 1, len(source) - 1)
        samples.append(source[left] + (source[right] - source[left]) * (position - left))
    opus = ReferenceOpus(opus_library)
    error = ctypes.c_int()
    encoder = opus.lib.opus_encoder_create(48000, 1, 2048, ctypes.byref(error))
    assert error.value == 0
    try:
        assert opus.lib.opus_encoder_ctl(encoder, 4002, 32000) == 0
        packets = []
        for start in range(0, len(samples) - 960, 960):
            pcm = (ctypes.c_float * 960)(*samples[start:start + 960])
            output = ctypes.create_string_buffer(1275)
            count = opus.lib.opus_encode_float(encoder, pcm, 960, output, 1275)
            assert count > 0
            packets.append(list(output.raw[:count]))
        return packets
    finally:
        opus.lib.opus_encoder_destroy(encoder)


def talk(client, packets):
    """Sends voice packets in real time, as a talking client would."""
    for index, packet in enumerate(packets):
        client.send(kind="voice", sequence=index * 2, opus=packet, end=index == len(packets) - 1)
        time.sleep(0.02)


def ducked_while_talking(link, client, packets):
    """Observe VAD state while sending silence or real encoded speech."""
    errors = []
    def speak():
        try:
            talk(client, packets)
        except Exception as error:
            errors.append(error)
    speaker = threading.Thread(target=speak, name="musicbot-speech")
    speaker.start()
    seen = False
    try:
        deadline = time.monotonic() + len(packets) * 0.02 + HTTP_TIMEOUT
        while speaker.is_alive():
            assert time.monotonic() < deadline, "speech sender timed out"
            seen |= api(link)["players"][0]["ducked"]
            time.sleep(0.1)
    finally:
        speaker.join(timeout=HTTP_TIMEOUT)
        assert not speaker.is_alive(), "speech sender did not stop"
    if errors:
        raise errors[0]
    return seen or api(link)["players"][0]["ducked"]




def audio_from(client, sender, seconds):
    """Voice packets from `sender` within `seconds`, until a transmission ends."""
    packets = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            event = client.wait(lambda event: event["kind"] == "audio" and event["sender"] == sender,
                                timeout=max(0.1, deadline - time.monotonic()))
        except AssertionError as error:
            if not str(error).startswith("Expected event missing:"):
                raise
            break
        packets.append(event)
        if event["end"]:
            break
    return packets


def drain(client):
    while not client.queue.empty():
        client.queue.get_nowait()


def session_of(client, name):
    # A fresh debug RSA identity can take longer than the network handshake.
    for _ in range(600):
        for user in list(client.users.values()):
            if user["name"] == name:
                return user["session"]
        time.sleep(0.1)
    raise AssertionError(f"{name} is not on the server: {client.users}")


def link_from(client, bot):
    """Asks `bot` for a web link with `!web`; it arrives privately."""
    client.send(kind="direct", session=bot, body="!web")
    text = client.wait(lambda event: event["kind"] == "text" and not event["outgoing"] and "/u/" in event["body"],
                       timeout=15)
    return re.search(r"https?://[^\s\"<]+/u/[0-9a-f-]{36}/", text["body"]).group(0)


def close_resources(resources):
    errors = []
    for resource in resources:
        if resource is not None:
            try:
                resource.close()
            except Exception as error:
                errors.append(error)
    if errors:
        raise errors[0]


def service(args):
    """Exercise the packaged NixOS bot without managing its service lifetime."""
    with tempfile.TemporaryDirectory(prefix="nuschl-musicbot-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, "127.0.0.1", args.port, root / "probe")
        listener = Client(args.binaries, "127.0.0.1", args.port, pin, "carol", root / "carol")
        watcher = None
        try:
            dj = session_of(listener, "DJ")
            link = link_from(listener, dj)
            state = api(link)
            assert state["you"] == "carol" and state["players"][0]["name"] == "root", state
            watcher = Events(link)
            watcher.wait(lambda state: state["you"] == "carol")
            command(link, "pause")
            command(link, "clear")
            command(link, "skip")
            command(link, "add", kind="song", library=0, id="s1", index=None)
            command(link, "add", kind="song", library=1, id="s2", index=None)
            watcher.wait(lambda state: len(state["players"][0]["queue"]) == 2)
            drain(listener)
            resumed = watcher.mark()
            command(link, "resume")
            heard = audio_from(listener, dj, 20)
            if len(heard) < 280:
                print(json.dumps({"service_errors": [
                    {"name": player["name"], "error": player["error"]}
                    for player in api(link)["players"]]}), file=sys.stderr)
            assert len(heard) >= 280 and heard[-1]["end"], len(heard)
            watcher.wait(lambda state: state["players"][0]["current"] is None
                         and not state["players"][0]["queue"], after=resumed)
            print(json.dumps({"service": len(heard)}))
        finally:
            close_resources([watcher, listener])


def write_config(config, root, library_url, web_port, port, pin, reverse=False, only_open=False):
    libraries = [f'''[[subsonic]]
name = "Plain"
url = "{library_url}/plain"
username = "bot"
password_file = "{root / 'password'}"
''', f'''[[subsonic]]
name = "Open"
url = "{library_url}/open"
api_key_file = "{root / 'key'}"
''']
    channels = [f'''[[channel]]
name = "{name}"
host = "127.0.0.1"
port = {port}
username = "{username}"
channel = "{channel}"
certificate_sha256 = "{pin}"
''' for name, username, channel in [("music", "DJ", "Music"), ("lobby", "DJ2", "Lobby")]]
    if only_open:
        libraries = libraries[1:]
    if reverse:
        libraries.reverse()
        channels.reverse()
    config.write_text(f'''[web]
listen = "127.0.0.1:{web_port}"
public_url = "http://127.0.0.1:{web_port}"

''' + "\n".join(libraries + channels))


class BotProcess:
    def __init__(self, args, config, root):
        invocation = [str(args.binaries / "nuschl-musicbot"), "--config", str(config),
                      "--state-directory", str(root / "state")]
        if args.web_assets is not None:
            invocation += ["--web-assets", str(args.web_assets)]
        self.log = deque(maxlen=200)
        self.process = subprocess.Popen(invocation, stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT, text=True)
        self.reader = threading.Thread(target=lambda: self.log.extend(self.process.stdout),
                                       name="musicbot-log")
        self.reader.start()

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
        try:
            code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=10)
            raise AssertionError("musicbot failed to exit after SIGTERM (SSE clients may be open)")
        finally:
            self.reader.join(timeout=5)
            self.process.stdout.close()
        assert not self.reader.is_alive(), "bot log reader did not stop"
        assert code == 0, f"musicbot exited {code}: {''.join(self.log)}"


def connected(client):
    sessions = [session_of(client, "DJ"), session_of(client, "DJ2")]
    for session, channel in zip(sessions, (MUSIC, LOBBY)):
        eventually(lambda: client.users.get(session),
                   lambda user: user is not None and user.get("channel") == channel,
                   f"bot {session} joining channel {channel}")
    return sessions


def presence_comments(client, sessions, link):
    public_url = link.split("/u/")[0] + "/"
    for session in sessions:
        eventually(lambda: client.users.get(session), lambda user:
                   user is not None and user.get("comment_hash"), "bot comment advertised")
        client.send(kind="blobs", comments=[session])
        user = eventually(lambda: client.users.get(session), lambda user:
                          user is not None and user.get("comment"), "bot comment retrieved")
        comment = html.unescape(user["comment"])
        assert f'href="{public_url}"' in comment, comment
        assert "!web" in comment and "private message" in comment, comment
        assert "/u/" not in comment and link not in comment, "public comment leaked a bearer link"


def channel_subscription_playback(link, alice, bob, dj):
    state = api(link)
    players = {player["name"]: index for index, player in enumerate(state["players"])}
    channels = {channel["name"]: index for index, channel in enumerate(state["channels"])}
    music, lobby = players["music"], players["lobby"]
    for player in (music, lobby):
        command(link, "pause", player=player)
        command(link, "clear", player=player)
        command(link, "skip", player=player)
    command(link, "attach", player=lobby, channel=channels["lobby"])
    command(link, "attach", player=music, channel=channels["music"])
    alice.send(kind="join", channel=LOBBY)
    eventually(lambda: alice.users.get(alice.session),
               lambda user: user is not None and user.get("channel") == LOBBY,
               "both users staying outside Music")
    eventually(lambda: api(link), lambda state:
               state["channels"][channels["music"]]["listeners"] == 0
               and state["players"][music]["listeners"] == 0, "empty Music channel")
    drain(alice), drain(bob)
    command(link, "resume", player=music)
    command(link, "add", player=music, kind="song", library=0, id="s2", index=None)
    waiting = eventually(lambda: api(link), lambda state:
                         state["players"][music]["current"] is not None,
                         "music waiting without listeners")
    assert waiting["players"][music]["current"]["position_ms"] == 0, waiting
    assert not audio_from(bob, dj, 0.3) and not audio_from(alice, dj, 0.3)
    bob.send(kind="listen", channel=MUSIC, listen=True)
    eventually(lambda: api(link), lambda state:
               state["channels"][channels["music"]]["listeners"] == 1
               and state["players"][music]["listeners"] == 1,
               "channel subscription counted as listener")
    heard = audio_from(bob, dj, 15)
    assert 140 <= len(heard) <= 160 and heard[-1]["end"], len(heard)
    assert not audio_from(alice, dj, 1), "an unsubscribed Lobby user heard Music"
    assert bob.users[bob.session]["channel"] == LOBBY, bob.users[bob.session]
    bob.send(kind="listen", channel=MUSIC, listen=False)
    eventually(lambda: api(link), lambda state:
               state["channels"][channels["music"]]["listeners"] == 0
               and state["players"][music]["listeners"] == 0,
               "removed channel subscription stops counting")
    return len(heard)


def rate(link, library, song, rating):
    request = urllib.request.Request(
        link + "api/rating", method="POST",
        data=json.dumps({"library": library, "id": song, "rating": rating}).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT) as response:
        assert response.status == 204 and response.read() == b"", response.status


def shared_ratings(link, watchers):
    """Ratings live in the library account; only a successful write bumps the
    live revision every viewer follows."""
    revision = api(link)["rating_revision"]
    assert api(link, "rating", library=0, id="s1") == {"rating": 0}
    rate(link, 0, "s1", 4)
    assert api(link, "rating", library=0, id="s1") == {"rating": 4}
    assert Library.ratings == {("plain", "s1"): 4}, Library.ratings
    for watcher in watchers:
        watcher.wait(lambda state: state["rating_revision"] == revision + 1)
    expect_error(link + "api/rating", 400, json_error=True,
                 data=json.dumps({"library": 0, "id": "s1", "rating": 6}).encode(),
                 headers={"Content-Type": "application/json"})
    rate(link, 0, "s1", 0)
    assert api(link, "rating", library=0, id="s1") == {"rating": 0} and not Library.ratings
    assert api(link)["rating_revision"] == revision + 2


def listening(link, player):
    return urllib.request.urlopen(link + f"api/listen?player={player}", timeout=HTTP_TIMEOUT)


def next_record(stream):
    """One browser-audio record: 0 reset, 1 PCM frame, 2 keep-alive."""
    kind = stream.read(1)
    assert len(kind) == 1, "the listening stream ended"
    if kind == b"\x01":
        payload = stream.read(7680)
        assert len(payload) == 7680, len(payload)
        samples = struct.unpack("<1920f", payload)
        assert all(math.isfinite(sample) and -1 <= sample <= 1 for sample in samples)
        return 1
    assert kind in (b"\x00", b"\x02"), kind
    return kind[0]


def new_link_from(client, bot, old):
    """`!web new` replaces the link; the old one stops working."""
    client.send(kind="direct", session=bot, body="!web new")
    text = client.wait(lambda event: event["kind"] == "text" and not event["outgoing"]
                       and "/u/" in event["body"] and old not in event["body"], timeout=15)
    return re.search(r"https?://[^\s\"<]+/u/[0-9a-f-]{36}/", text["body"]).group(0)


def browser_listening(link, alice, dj, player):
    """A browser alone plays a queue with nobody in its channel; streams are
    bounded and end when their link is replaced."""
    assert api(link)["players"][player]["listeners"] == 0
    stream = listening(link, player)
    try:
        headers = stream.headers
        assert headers.get_content_type() == "application/octet-stream", dict(headers)
        assert headers["Cache-Control"] == "no-store" and headers["X-Accel-Buffering"] == "no"
        assert (headers["X-Audio-Format"], headers["X-Audio-Rate"], headers["X-Audio-Channels"],
                headers["X-Audio-Frame-Samples"]) == ("f32le", "48000", "2", "960"), dict(headers)
        assert stream.read(1) == b"\x00", "the stream did not start with a reset"
        eventually(lambda: api(link), lambda state: state["players"][player]["listeners"] == 1,
                   "browser listener counted")
        command(link, "add", player=player, kind="song", library=0, id="s1", index=None)
        frames = 0
        while frames < 25:
            frames += next_record(stream) == 1
        extra = []
        try:
            for _ in range(15):
                extra.append(listening(link, player))
            expect_error(link + f"api/listen?player={player}", 503, json_error=True)
        finally:
            close_resources(extra)
        replaced = new_link_from(alice, dj, link)
        deadline = time.monotonic() + 10
        while stream.read(65536):
            assert time.monotonic() < deadline, "a replaced link kept streaming"
        expect_error(link + f"api/listen?player={player}", 404, json_error=True)
        return replaced, frames
    finally:
        stream.close()


def track_identity(state, entry):
    track = entry["track"]
    return state["libraries"][track["library"]], track["id"], entry["added_by"]


def paused_current(link, listener, sender):
    listener.wait(lambda event: event["kind"] == "audio" and event["sender"] == sender, timeout=15)
    command(link, "pause")
    state = eventually(lambda: api(link), lambda state: state["players"][0]["paused"]
                       and state["players"][0]["current"] is not None, "paused current track")
    stopped = audio_from(listener, sender, 5)
    assert stopped and stopped[-1]["end"], stopped[-1:]
    drain(listener)
    return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("port", type=int)
    parser.add_argument("--service", action="store_true",
                        help="test the running packaged bot (DJ in the root channel)")
    parser.add_argument("--espeak", type=Path, help="espeak-ng for the ducking test")
    parser.add_argument("--opus-library", type=Path, help="reference libopus for speech encoding")
    parser.add_argument("--web-assets", type=Path, help="built UI directory; forwarded only when supplied")
    args = parser.parse_args()
    if not args.service and not (args.espeak and args.opus_library):
        parser.error("--espeak and --opus-library are needed unless --service")
    if args.service:
        service(args)
        return

    library = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Library)
    library_thread = threading.Thread(target=library.serve_forever, name="fake-subsonic")
    library_thread.start()
    library_url = f"http://127.0.0.1:{library.server_address[1]}"
    try:
        with tempfile.TemporaryDirectory(prefix="nuschl-musicbot-") as temporary:
            root = Path(temporary)
            pin = certificate_pin(args.binaries, "127.0.0.1", args.port, root / "probe")
            (root / "password").write_text(PASSWORD + "\n")
            (root / "key").write_text(API_KEY + "\n")
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                web_port = probe.getsockname()[1]
            config = root / "musicbot.toml"
            write_config(config, root, library_url, web_port, args.port, pin)
            bot = BotProcess(args, config, root)
            alice = bob = None
            watchers = []
            try:
                alice = Client(args.binaries, "127.0.0.1", args.port, pin, "alice", root / "alice")
                bob = Client(args.binaries, "127.0.0.1", args.port, pin, "bob", root / "bob")
                alice.send(kind="join", channel=MUSIC)
                bob.send(kind="join", channel=LOBBY)
                dj, dj2 = connected(alice)
                link, bob_link = link_from(alice, dj), link_from(bob, dj2)
                presence_comments(alice, (dj, dj2), link)
                assert link != bob_link, "viewers received the same secret token"
                state = api(link)
                assert state["you"] == "alice" and state["libraries"] == ["Plain", "Open"], state
                assert [player["name"] for player in state["players"]] == ["music", "lobby"], state
                with urllib.request.urlopen(link, timeout=HTTP_TIMEOUT) as response:
                    assert response.status == 200
                    assert response.headers["Referrer-Policy"] == "no-referrer", dict(response.headers)
                invalid = link.split("/u/")[0] + "/u/00000000-0000-0000-0000-000000000000/"
                for endpoint in ("", "api/state", "api/events", "api/history?player=0", "api/search?q=sine", "art/0/missing"):
                    expect_error(invalid + endpoint, 404)
                expect_error(invalid + "api/command", 404,
                             data=b'{"command":"pause","player":0}',
                             headers={"Content-Type": "application/json"}, json_error=True)
                expect_error(link + "api/command", 400,
                             data=b'{"command":"pause","player":999}',
                             headers={"Content-Type": "application/json"}, json_error=True)
                assert api(link, "history", player=0)["items"] == []
                expect_error(link + "api/history?player=999", 400, json_error=True)
                expect_error(link + "api/history?player=0&limit=0", 400, json_error=True)
                browse(link)

                watchers.append(Events(link))
                watchers.append(Events(bob_link))
                for watcher, name in zip(watchers, ("alice", "bob")):
                    watcher.wait(lambda state, name=name: state["you"] == name)
                shared_ratings(link, watchers)
                # Keep the queue paused so exact shared snapshots can be compared.
                broadcast(watchers, link, "pause", lambda state: state["players"][0]["paused"])
                broadcast(watchers, bob_link, "volume", lambda state: state["players"][0]["volume"] == 71,
                          volume=71)
                shared_state = broadcast(watchers, link, "add",
                    lambda state: len(state["players"][0]["queue"]) == 2,
                    kind="album", library=0, id="a1", index=None)
                assert [entry["track"]["id"] for entry in shared_state["players"][0]["queue"]] == ["s1", "s2"]
                shared_state = broadcast(watchers, bob_link, "add",
                    lambda state: len(state["players"][0]["queue"]) == 3,
                    kind="song", library=1, id="s1", index=0)
                queue = shared_state["players"][0]["queue"]
                assert queue[0]["added_by"] == "bob" and queue[1]["added_by"] == "alice", queue
                moved = queue[0]["id"]
                shared_state = broadcast(watchers, bob_link, "move",
                    lambda state: state["players"][0]["queue"][-1]["id"] == moved,
                    entry=moved, index=2)
                assert [entry["id"] for entry in shared_state["players"][0]["queue"]] == [queue[1]["id"], queue[2]["id"], moved]
                removed = broadcast(watchers, link, "remove", lambda state: len(state["players"][0]["queue"]) == 2,
                                    entry=queue[2]["id"])
                assert [entry["id"] for entry in removed["players"][0]["queue"]] == [queue[1]["id"], moved]
                broadcast(watchers, link, "clear", lambda state: not state["players"][0]["queue"])
                assert api(link, "history", player=0)["items"] == [], "upcoming tracks entered history"
                broadcast(watchers, link, "volume", lambda state: state["players"][0]["volume"] == 100, volume=100)
                broadcast(watchers, link, "resume", lambda state: not state["players"][0]["paused"])

                # Full three-second track is the measured boundary for seek tests.
                drain(alice), drain(bob)
                command(link, "add", kind="song", library=0, id="s1", index=None)
                full = audio_from(alice, dj, 15)
                assert 140 <= len(full) <= 160 and full[-1]["end"], len(full)
                assert not audio_from(bob, dj2, 1), "Lobby played without a song"
                completed = eventually(lambda: api(link, "history", player=0),
                    lambda page: any(item["outcome"] == "completed" for item in page["items"]),
                    "completed play in history")["items"][0]
                assert completed["entry"]["track"]["id"] == "s1"
                assert completed["entry"]["added_by"] == "alice"
                assert completed["library_name"] == "Plain"
                assert completed["started_at_ms"] is not None and completed["error"] is None
                assert 2800 <= completed["played_ms"] <= 3200, completed
                first_history_id = completed["id"]
                # The library account hears of the play: now playing, then one submission.
                reports = eventually(lambda: [report for report in Library.scrobbles if report[1] == "s1"],
                                     lambda reports: any(report[3] for report in reports),
                                     "playback submitted to the library")
                assert reports == [("plain", "s1", completed["started_at_ms"], False),
                                   ("plain", "s1", completed["started_at_ms"], True)], reports

                drain(alice)
                command(link, "add", kind="song", library=0, id="s1", index=None)
                paused_current(link, alice, dj)
                assert api(link, "history", player=0)["items"][0]["id"] == first_history_id
                command(link, "seek", position_ms=1500)
                seeked = eventually(lambda: api(link), lambda state:
                    state["players"][0]["current"]["position_ms"] == 1500, "seek to 1500 ms")
                assert not seeked["players"][0]["current"]["advancing"], seeked
                time.sleep(0.25)
                assert api(link)["players"][0]["current"]["position_ms"] == 1500
                command(link, "resume")
                remainder = audio_from(alice, dj, 15)
                assert 60 <= len(remainder) <= 90 and remainder[-1]["end"], len(remainder)
                assert len(remainder) < len(full) - 45, (len(full), len(remainder))
                seek_history = eventually(lambda: api(link, "history", player=0),
                    lambda page: page["items"][0]["id"] > first_history_id,
                    "seeked play finishes once")["items"]
                assert len(seek_history) == 2 and seek_history[0]["outcome"] == "completed", seek_history
                assert 1200 <= seek_history[0]["played_ms"] < 2400, seek_history[0]
                cursor_page = api(link, "history", player=0, limit=1)
                assert cursor_page["next_before"] == seek_history[0]["id"], cursor_page
                older_page = api(link, "history", player=0, before=cursor_page["next_before"], limit=1)
                assert [item["id"] for item in older_page["items"]] == [first_history_id], older_page
                assert older_page["next_before"] is None

                # VAD detects speech, not silence, and respects its disabled setting.
                speech = speech_packets(args.espeak, args.opus_library, root)
                assert len(speech) > 100, len(speech)
                alice.unmute()
                drain(alice)
                command(link, "ducking", enabled=True, level=30, hold_ms=800, sensitivity=50)
                for _ in range(4):
                    command(link, "add", kind="album", library=0, id="a1", index=None)
                alice.wait(lambda event: event["kind"] == "audio" and event["sender"] == dj, timeout=15)
                assert not ducked_while_talking(link, alice, [SILENT_OPUS] * 50), "silent microphone ducked"
                assert ducked_while_talking(link, alice, speech), "speech did not duck music"
                disabled = {"enabled": False, "level": 20, "hold_ms": 500, "sensitivity": 50}
                command(link, "ducking", **disabled)
                assert api(link)["players"][0]["ducking"] == disabled
                eventually(lambda: api(link), lambda state: not state["players"][0]["ducked"], "VAD release")
                assert not ducked_while_talking(link, alice, speech), "disabled VAD ducked"
                command(link, "clear")
                command(link, "skip")
                audio_from(alice, dj, 15)

                skipped = eventually(lambda: api(link, "history", player=0),
                    lambda page: page["items"][0]["outcome"] == "skipped",
                    "skipped playback retained")["items"][0]
                assert skipped["started_at_ms"] is not None and skipped["played_ms"] > 0, skipped
                # Both channels receive identical encoded packets from one queue.
                command(link, "attach", channel=1)
                eventually(lambda: api(link), lambda state: state["players"][0]["listeners"] == 2,
                           "both channel subscriptions")
                drain(alice), drain(bob)
                history_before_shared = api(link, "history", player=0)["revision"]
                command(link, "add", kind="album", library=1, id="a1", index=None)
                heard_alice = audio_from(alice, dj, 20)
                heard_bob = audio_from(bob, dj2, 5)
                assert len(heard_alice) >= 280 and len(heard_bob) >= 280, (len(heard_alice), len(heard_bob))
                assert heard_alice[-1]["end"] and heard_bob[-1]["end"]
                packets = [Counter(bytes(packet["opus"]) for packet in heard) for heard in (heard_alice, heard_bob)]
                shared = sum((packets[0] & packets[1]).values())
                assert shared >= 270, (shared, len(heard_alice), len(heard_bob))
                shared_history = eventually(lambda: api(link, "history", player=0),
                    lambda page: page["revision"] == history_before_shared + 2,
                    "shared-channel songs each recorded once")
                assert [item["entry"]["track"]["id"] for item in shared_history["items"][:2]] == ["s2", "s1"]
                assert all(item["library_name"] == "Open" for item in shared_history["items"][:2])
                assert Library.decisions and Library.decisions[0]["directPlayProfiles"], Library.decisions

                command(link, "attach", player=1, channel=1)
                eventually(lambda: api(link), lambda state: state["players"][1]["listeners"] == 1,
                           "separate Lobby queue subscription")
                # The actor queues an explicit end marker when changing its
                # subscription. The API's listener count is not a network ACK:
                # observe that boundary before starting the next transmission,
                # rather than racing it with a best-effort queue drain.
                boundary = bob.wait(lambda event: event["kind"] == "audio"
                    and event["sender"] == dj2 and event["end"]
                    and event["opus"] == SILENT_OPUS, timeout=5)
                assert boundary["opus"] == SILENT_OPUS
                drain(alice), drain(bob)
                command(link, "add", player=1, kind="playlist", library=0, id="p1", index=0)
                heard = audio_from(bob, dj2, 20)
                assert len(heard) >= 280 and heard[-1]["end"], len(heard)
                assert not audio_from(alice, dj, 1), "Music played the Lobby queue"
                eventually(lambda: api(link), lambda state: all(player["current"] is None
                    and not player["queue"] for player in state["players"]), "both queues finished")

                assert [item["entry"]["track"]["id"] for item in api(link, "history", player=1)["items"]] == ["s1", "s2"]
                failure_revision = api(link, "history", player=0)["revision"]
                Library.failed_streams.add(("plain", "s1"))
                try:
                    command(link, "add", kind="song", library=0, id="s1", index=None)
                    failed = eventually(lambda: api(link, "history", player=0),
                        lambda page: page["revision"] == failure_revision + 1,
                        "failed stream retained")["items"][0]
                    assert failed["outcome"] == "failed" and failed["error"], failed
                    assert failed["started_at_ms"] is None and failed["played_ms"] == 0, failed
                finally:
                    Library.failed_streams.discard(("plain", "s1"))
                command(link, "add", kind="song", library=1, id="s2", index=None)
                recovered = eventually(lambda: api(link, "history", player=0),
                    lambda page: page["revision"] == failure_revision + 2,
                    "next song after failure recorded")
                assert recovered["items"][0]["outcome"] == "completed", recovered
                assert recovered["items"][1]["id"] == failed["id"] and recovered["items"][1]["error"]
                assert api(link)["players"][0]["error"] is None
                for watcher in watchers:
                    watcher.wait(lambda state: state["players"][0]["history_revision"] == recovered["revision"])
                # Deploy with a paused current song and mixed-library upcoming order.
                drain(alice)
                command(link, "add", kind="song", library=0, id="s1", index=None)
                paused_current(link, alice, dj)
                for library_index, song in [(1, "s2"), (0, "s2"), (1, "s1")]:
                    command(bob_link, "add", kind="song", library=library_index, id=song, index=None)
                command(link, "seek", position_ms=1200)
                command(link, "volume", volume=63)
                settings = {"enabled": False, "level": 17, "hold_ms": 1234, "sensitivity": 67}
                command(link, "ducking", **settings)
                command(link, "attach", channel=1)
                before = eventually(lambda: api(link), lambda state:
                    state["players"][0]["current"]["position_ms"] == 1200
                    and state["players"][0]["listeners"] == 2, "deployment snapshot")
                expected_current = track_identity(before, before["players"][0]["current"]["entry"])
                expected_queue = [track_identity(before, entry) for entry in before["players"][0]["queue"]]
                assert expected_current == ("Plain", "s1", "alice"), expected_current
                assert expected_queue == [("Open", "s2", "bob"), ("Plain", "s2", "bob"),
                                          ("Open", "s1", "bob")], expected_queue
                history_before_restart = api(link, "history", player=0)
                for watcher in watchers:
                    watcher.wait(lambda state: state["players"][0]["history_revision"]
                                 == history_before_restart["revision"])
                for watcher in watchers:
                    watcher.wait(lambda state: state["players"][0]["paused"]
                                 and state["players"][0]["volume"] == 63
                                 and state["players"][0]["ducking"] == settings
                                 and len(state["players"][0]["queue"]) == 3)
                # Leave both HTTP readers open during SIGTERM: shutdown must not hang.
                bot.stop()
                saved_current = json.loads((root / "state" / "queues.json").read_text())["players"][0]["current"]
                assert saved_current["started_at_ms"] is not None and saved_current["played_ms"] > 0
                for watcher in watchers:
                    watcher.close()
                watchers.clear()
                eventually(lambda: dict(alice.users), lambda users: dj not in users and dj2 not in users,
                           "old bot sessions disconnecting")
                write_config(config, root, library_url, web_port, args.port, pin, reverse=True)
                bot = BotProcess(args, config, root)
                dj, dj2 = connected(alice)
                link, bob_link = link_from(alice, dj), link_from(bob, dj2)
                watchers.append(Events(link))
                presence_comments(alice, (dj, dj2), link)
                watchers.append(Events(bob_link))
                after = api(link)
                assert after["libraries"] == ["Open", "Plain"], after
                # Covers decoded before the restart come from disk. The art key
                # belongs to the library, so the swapped index does not matter.
                asked = [seen for seen in Library.seen if seen[1] == "getCoverArt"]
                cover = f"art/1/{urllib.parse.quote(COVER, safe='')}?size=300&v={after['art_keys'][1]}"
                with urllib.request.urlopen(link + cover, timeout=HTTP_TIMEOUT) as response:
                    assert response.headers["Cache-Control"] == "private, max-age=86400", dict(response.headers)
                assert [seen for seen in Library.seen if seen[1] == "getCoverArt"] == asked, "covers were not kept on disk"
                music = next(index for index, player in enumerate(after["players"]) if player["name"] == "music")
                restored = after["players"][music]
                assert restored["paused"] and restored["volume"] == 63 and restored["ducking"] == settings, restored
                assert restored["current"]["position_ms"] == 1200 and not restored["current"]["advancing"], restored
                assert track_identity(after, restored["current"]["entry"]) == expected_current, restored
                assert [track_identity(after, entry) for entry in restored["queue"]] == expected_queue, restored
                history_after_restart = api(link, "history", player=music)
                assert history_after_restart["revision"] == history_before_restart["revision"]
                assert [item["id"] for item in history_after_restart["items"]] == [
                    item["id"] for item in history_before_restart["items"]]
                for item in history_after_restart["items"]:
                    assert after["libraries"][item["entry"]["track"]["library"]] == item["library_name"], item
                assert [channel["name"] for channel in after["channels"]] == ["lobby", "music"], after
                assert all(after["players"][channel["player"]]["name"] == "music" for channel in after["channels"]), after
                eventually(lambda: api(link), lambda state: state["players"][music]["listeners"] == 2,
                           "restored channel attachment")
                for watcher in watchers:
                    watcher.wait(lambda state: state["players"][music]["paused"]
                                 and state["players"][music]["current"]["position_ms"] == 1200)
                assert not audio_from(alice, dj, 0.3) and not audio_from(bob, dj2, 0.3), "restored paused music played"
                # Isolate the current remainder after proving upcoming order survived.
                command(link, "clear", player=music)
                drain(alice), drain(bob)
                command(link, "resume", player=music)
                deployed_audio = audio_from(alice, dj, 15)
                deployed_bob = audio_from(bob, dj2, 5)
                assert 75 <= len(deployed_audio) <= 105 and deployed_audio[-1]["end"], len(deployed_audio)
                assert 75 <= len(deployed_bob) <= 105 and deployed_bob[-1]["end"], len(deployed_bob)
                assert len(deployed_audio) < len(full) - 30, (len(full), len(deployed_audio))
                deployed_history = eventually(lambda: api(link, "history", player=music),
                    lambda page: page["revision"] == history_before_restart["revision"] + 1,
                    "resumed play finishes once")["items"][0]
                assert deployed_history["started_at_ms"] == saved_current["started_at_ms"], deployed_history
                assert deployed_history["played_ms"] > saved_current["played_ms"], deployed_history
                assert deployed_history["entry"]["track"]["id"] == "s1" and deployed_history["library_name"] == "Plain"
                assert all(player["error"] is None for player in api(link)["players"])
                authentications = {(name, how) for name, _, how in Library.seen}
                assert authentications == {("plain", "token"), ("open", "apiKey")}, authentications
                retained_revision = api(link, "history", player=music)["revision"]
                bot.stop()
                for watcher in watchers:
                    watcher.close()
                watchers.clear()
                eventually(lambda: dict(alice.users), lambda users: dj not in users and dj2 not in users,
                           "bot sessions disconnecting before library removal")
                write_config(config, root, library_url, web_port, args.port, pin, reverse=True, only_open=True)
                bot = BotProcess(args, config, root)
                connected(alice)
                removed_library_history = api(link, "history", player=music)
                assert removed_library_history["revision"] == retained_revision
                assert api(link)["libraries"] == ["Open"]
                for item in removed_library_history["items"]:
                    expected_library = None if item["library_name"] == "Plain" else 0
                    assert item["entry"]["track"]["library"] == expected_library, item
                assert any(item["library_name"] == "Plain" and item["entry"]["track"]["title"]
                           for item in removed_library_history["items"]), removed_library_history
                presence_comments(alice, connected(alice), link)
                subscription_audio = channel_subscription_playback(link, alice, bob, session_of(alice, "DJ"))
                music_bot = session_of(alice, "DJ")
                link, browser_frames = browser_listening(link, alice, music_bot, music)
                print(json.dumps({"full_song": len(full), "seek_remainder": len(remainder),
                                  "music": len(heard_alice), "lobby": len(heard_bob), "shared": shared,
                                  "deployment_remainder": len(deployed_audio), "sse_viewers": 2,
                                  "music_history": deployed_history["id"], "history_pagination": True,
                                  "channel_subscription_audio": subscription_audio,
                                  "browser_frames": browser_frames, "scrobbles": len(Library.scrobbles)}))
            finally:
                try:
                    bot.stop()
                finally:
                    if sys.exc_info()[0]:
                        print("".join(bot.log), file=sys.stderr)
                    close_resources([*watchers, alice, bob])
    finally:
        library.shutdown()
        library.server_close()
        library_thread.join(timeout=5)
        assert not library_thread.is_alive(), "fake Subsonic server did not stop"




if __name__ == "__main__":
    main()
