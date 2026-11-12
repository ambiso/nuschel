# SPDX-License-Identifier: EUPL-1.2
"""ExecStartPre inside the actual relay cgroup, never a host executable."""
import ipaddress
import json
from pathlib import Path
import socket
import sys

resolver, expected = sys.argv[1:]
addresses = {item[4][0] for item in socket.getaddrinfo("voice.dns.test", 64738, socket.AF_INET, socket.SOCK_STREAM)}
assert addresses == {expected}, addresses
assert ipaddress.ip_address(expected).is_private
# The DNS address exception really permits ALL ports at that address, not just
# 53. The application must still reject it as a tunnel destination below.
with socket.create_connection((resolver, 64738), timeout=2):
    pass
blocked = []
for address in ["127.0.0.1", "127.0.0.56"]:
    try:
        with socket.create_connection((address, 64738), timeout=1):
            raise AssertionError(f"Kernel IP filter permitted {address}")
    except (TimeoutError, PermissionError, ConnectionError):
        blocked.append(address)
Path("/run/nuschl-web-relay/dns-filter-proof.json").write_text(json.dumps({
    "resolver": resolver, "dns_name": "voice.dns.test", "addresses": sorted(addresses),
    "dns_address_other_port_reachable": True, "other_loopbacks_blocked": blocked,
    "scope": "actual relay unit ExecStartPre/cgroup; owned guest services only",
    "cgroup": Path("/proc/self/cgroup").read_text(),
}) + "\n")
