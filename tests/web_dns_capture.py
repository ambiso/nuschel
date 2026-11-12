# SPDX-License-Identifier: EUPL-1.2
"""Bounded guest-root header counter; never stores captured packet contents."""
import json
from pathlib import Path
import socket
import sys
import time

address, output, ready = sys.argv[1:]
destination = socket.inet_aton(address)
count = 0
with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0800)) as packets:
    packets.settimeout(.1)
    deadline = time.monotonic() + 2
    Path(ready).write_text("ready\n")
    while time.monotonic() < deadline:
        try:
            frame = packets.recv(65535)
        except TimeoutError:
            continue
        # Linux loopback raw frames have the ordinary14-byte Ethernet header.
        # Read only IPv4 destination/protocol fields. No payload is retained.
        if len(frame) >= 34 and frame[12:14] == b"\x08\x00" and frame[14] >> 4 == 4:
            if frame[30:34] == destination and frame[23] in (6, 17):
                count += 1
Path(output).write_text(json.dumps({"destination":address,"TCP_UDP_packets":count,
    "seconds":2,"interface":"owned guest virtual interfaces","payload_stored":False})+"\n")
