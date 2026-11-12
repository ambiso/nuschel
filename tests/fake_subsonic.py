#!/usr/bin/env python3
"""A fake music library for the music bot's tests, on loopback.

Two libraries live under one HTTP server: `/plain` is a plain Subsonic server
(password token authentication, original files) and `/open` an OpenSubsonic
one (API key, the transcoding extension). Both have two three-second sine
songs on one album. Run directly, it serves on the given port.
"""
import argparse
import hashlib
import http.server
import json
import math
import struct
import urllib.parse
import zlib

PASSWORD = "library-secret"
API_KEY = "open-key-123"


def wav(seconds, rate=44_100, frequency=440.0):
    frames = int(seconds * rate)
    data = bytearray()
    for n in range(frames):
        sample = int(math.sin(2 * math.pi * frequency * n / rate) * 0.5 * 32767)
        data += struct.pack("<hh", sample, sample)
    header = b"RIFF" + struct.pack("<I", 36 + len(data)) + b"WAVEfmt "
    header += struct.pack("<IHHIIHH", 16, 1, 2, rate, rate * 4, 4, 16)
    return header + b"data" + struct.pack("<I", len(data)) + bytes(data)


def png(red, green, blue):
    """A 1x1 PNG of one colour."""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    header = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
    pixels = zlib.compress(bytes([0, red, green, blue]))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", pixels) + chunk(b"IEND", b"")


# The cover's id has a slash, as some servers' ids do, to check the escaping.
COVER = "al/a1"
COVER_PNG = png(200, 80, 40)
ALBUM = {"id": "a1", "name": "Waves", "artist": "Oscillators", "artistId": "ar1", "year": 2026,
         "songCount": 2, "duration": 6, "coverArt": COVER}
ARTIST = {"id": "ar1", "name": "Oscillators", "albumCount": 1, "coverArt": COVER}
PLAYLIST = {"id": "p1", "name": "Backwards", "songCount": 2, "duration": 6, "owner": "ada", "coverArt": COVER}
SONGS = {
    "s1": {"id": "s1", "title": "Sine in A", "artist": "Oscillators", "album": "Waves", "track": 1,
           "albumId": "a1", "artistId": "ar1", "coverArt": COVER,
           "duration": 3, "suffix": "wav", "contentType": "audio/wav"},
    "s2": {"id": "s2", "title": "Sine in E", "artist": "Oscillators", "album": "Waves", "track": 2,
           "albumId": "a1", "artistId": "ar1", "coverArt": COVER,
           "duration": 3, "suffix": "wav", "contentType": "audio/wav"},
}
AUDIO = {"s1": wav(3, frequency=440.0), "s2": wav(3, frequency=659.3)}


class Library(http.server.BaseHTTPRequestHandler):
    seen = []  # (library, endpoint, authentication)
    decisions = []
    failed_streams = set()  # (library, song) whose metadata exists but audio fails

    def log_message(self, *_):
        pass

    def reply(self, body):
        payload = json.dumps({"subsonic-response": {"status": "ok", "version": "1.16.1", **body}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def failure(self, code, message):
        payload = json.dumps({"subsonic-response": {"status": "failed", "version": "1.16.1",
                                                    "error": {"code": code, "message": message}}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(payload)

    def audio(self, song):
        library = urllib.parse.urlparse(self.path).path.strip("/").split("/")[0]
        if (library, song) in self.failed_streams:
            self.failure(70, "audio is temporarily unavailable")
            return
        body = AUDIO[song]
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def authenticated(self, library, query):
        if library == "open":
            if query.get("apiKey") != [API_KEY] or "u" in query:
                return "wrong api key"
            return "apiKey"
        salt = query.get("s", [""])[0]
        token = hashlib.md5((PASSWORD + salt).encode()).hexdigest()
        if query.get("u") != ["bot"] or query.get("t") != [token] or "p" in query:
            return "wrong token"
        return "token"

    def handle_request(self):
        url = urllib.parse.urlparse(self.path)
        query = urllib.parse.parse_qs(url.query)
        parts = url.path.strip("/").split("/")
        if len(parts) != 3 or parts[1] != "rest" or parts[0] not in ("plain", "open"):
            self.send_error(404)
            return
        library, endpoint = parts[0], parts[2]
        authentication = self.authenticated(library, query)
        Library.seen.append((library, endpoint, authentication))
        if authentication.startswith("wrong"):
            self.failure(40, authentication)
            return
        if endpoint == "getOpenSubsonicExtensions":
            if library == "plain":
                self.failure(70, "unknown endpoint")
            else:
                self.reply({"openSubsonic": True, "openSubsonicExtensions": [
                    {"name": "apiKeyAuthentication", "versions": [1]},
                    {"name": "transcoding", "versions": [1]}]})
        elif endpoint == "ping":
            self.reply({})
        elif endpoint == "search3":
            hits = [song for song in SONGS.values() if query["query"][0].lower() in song["title"].lower()]
            self.reply({"searchResult3": {"song": hits, "album": [ALBUM], "artist": [ARTIST]}})
        elif endpoint == "getSong":
            self.reply({"song": SONGS[query["id"][0]]})
        elif endpoint == "getAlbum":
            self.reply({"album": {**ALBUM, "song": list(SONGS.values())}})
        elif endpoint == "getAlbumList2":
            self.reply({"albumList2": {"album": [ALBUM] if query.get("offset") == ["0"] else []}})
        elif endpoint == "getArtists":
            self.reply({"artists": {"ignoredArticles": "The", "index": [{"name": "O", "artist": [ARTIST]}]}})
        elif endpoint == "getArtist":
            self.reply({"artist": {**ARTIST, "album": [ALBUM]}})
        elif endpoint == "getPlaylists":
            self.reply({"playlists": {"playlist": [PLAYLIST]}})
        elif endpoint == "getPlaylist":
            self.reply({"playlist": {**PLAYLIST, "entry": [SONGS["s2"], SONGS["s1"]]}})
        elif endpoint == "getCoverArt":
            if query.get("id") != [COVER]:
                self.failure(70, "no such cover")
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(COVER_PNG)))
            self.end_headers()
            self.wfile.write(COVER_PNG)
        elif endpoint == "stream" and library == "plain":
            self.audio(query["id"][0])
        elif endpoint == "getTranscodeDecision" and library == "open" and self.command == "POST":
            length = int(self.headers.get("Content-Length", "0"))
            Library.decisions.append(json.loads(self.rfile.read(length)))
            self.reply({"transcodeDecision": {
                "canDirectPlay": False, "canTranscode": True, "transcodeReason": ["ContainerNotSupported"],
                "transcodeParams": "fake-" + query["mediaId"][0],
                "transcodeStream": {"protocol": "http", "container": "wav", "codec": "pcm"}}})
        elif endpoint == "getTranscodeStream" and library == "open":
            assert query["transcodeParams"] == ["fake-" + query["mediaId"][0]], query
            self.audio(query["mediaId"][0])
        else:
            self.send_error(400)

    do_GET = handle_request
    do_POST = handle_request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Library).serve_forever()


if __name__ == "__main__":
    main()
