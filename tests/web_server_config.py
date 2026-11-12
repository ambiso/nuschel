#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent loopback TLS configuration changes while real App drafts remain."""

import argparse
import json
import os
import socket
import struct
import subprocess
from pathlib import Path

from local_murmur import eventually, free_port
from web_live_welcome import Server, integer, text


class ConfigurationServer(Server):
    def __init__(self, root, openssl):
        self.control_file, self.applied, self.messages, self.changes = (
            root / "control.json",
            {},
            [],
            [],
        )
        self.hold_ack = root / "hold-wire-ack"
        self.pending_acks = {}
        self.received_file = root / "wire-received.json"
        super().__init__(root, openssl)

    def initial_limits(self):
        return None  # Exercise real per-connection defaults before explicit changes.

    def poll_control(self, stream):
        if not self.hold_ack.exists() and stream in self.pending_acks:
            stream.sendall(self.pending_acks.pop(stream))
        if not self.control_file.exists():
            return
        assert self.control_file.stat().st_size <= 4096
        value = json.loads(self.control_file.read_text())
        seq, updates = value["seq"], value["fields"]
        assert isinstance(seq, int) and 0 < seq <= 32
        if self.applied.get(stream, 0) >= seq:
            return
        assert len(updates) <= 6
        body = bytearray()
        for key, item in updates.items():
            key = int(key)
            assert (
                key in {1, 3, 4, 5, 6, 7}
                and isinstance(item, int)
                and 0 <= item <= 2**32 - 1
            )
            body.extend(integer(key, item))
        stream.sendall(struct.pack(">HI", 24, len(body)) + body)
        ack = integer(1, 3) + integer(3, 0) + text(5, f"config-applied:{seq}")
        stream.sendall(struct.pack(">HI", 11, len(ack)) + ack)
        self.applied[stream] = seq
        self.changes.append(value)

    def handle_text(self, stream, body):
        assert len(body) <= 8192
        self.messages.append(
            {
                "text": body.decode(),
                "bytes": len(body),
                "utf16_units": len(body.decode().encode("utf-16-le")) // 2,
            }
        )
        ack = integer(1, 3) + integer(3, 0) + text(5, f"wire-ack:{len(self.messages)}")
        packet = struct.pack(">HI", 11, len(ack)) + ack
        self.received_file.write_text(json.dumps({"messages": len(self.messages)}))
        if self.hold_ack.exists():
            assert stream not in self.pending_acks
            self.pending_acks[stream] = packet
        else:
            stream.sendall(packet)
        return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in (
        "site",
        "relay",
        "openssl",
        "wrapper",
        "browser-tmp",
        "playwright",
        "output",
    ):
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    fixture = ConfigurationServer(args.output, args.openssl)
    port = free_port()
    config = args.output / "relay.toml"
    config.write_text(
        f'listen="127.0.0.1:{port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n'
    )
    with (args.output / "relay.log").open("w") as log:
        relay = subprocess.Popen(
            [str(args.relay), "--config", str(config)], stdout=log, stderr=log
        )
        try:

            def listening():
                with socket.socket() as probe:
                    return probe.connect_ex(("127.0.0.1", port)) == 0

            eventually(listening, "private relay failed to start")
            subprocess.run(
                [
                    "node",
                    str(Path(__file__).with_suffix(".browser.mjs")),
                    str(args.site),
                    f"ws://127.0.0.1:{port}/relay",
                    str(fixture.port),
                    str(args.wrapper),
                    str(args.playwright),
                    str(args.output),
                ],
                env={**os.environ, "TMPDIR": str(args.browser_tmp)},
                timeout=100,
                check=True,
            )
            expected = [
                "x" * 129,
                "&lt;&amp;&gt;<br/>",
                "<&>\n",
                "é",
                "🙂",
                "漢字",
                "🙂",
                "x" * 128,
                "x" * 5001,
                'see <a href="https://example.test/">https://example.test/</a>',
                "<br/>```<br/>x<br/>```",
                "x" * 5000,
            ]
            assert [value["text"] for value in fixture.messages] == expected, (
                fixture.messages
            )
            assert [value["utf16_units"] for value in fixture.messages] == [
                len(value.encode("utf-16-le")) // 2 for value in expected
            ]

        finally:
            relay.terminate()
            relay.wait(timeout=10)
            fixture.close()
            (args.output / "wire-proof.json").write_text(
                json.dumps(
                    {
                        "messages": fixture.messages,
                        "changes": fixture.changes,
                        "errors": fixture.errors,
                    },
                    indent=2,
                )
                + "\n"
            )
            assert not fixture.errors, fixture.errors


if __name__ == "__main__":
    main()
