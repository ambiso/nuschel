# Nuschel patch to magic-wormhole 0.8.1

Upstream: <https://github.com/magic-wormhole/magic-wormhole.rs>, crates.io
release 0.8.1. The upstream EUPL-1.2 license and source are retained.

`src/transit.rs`: when direct transit is disabled (`FORCE_RELAY`), use only
the caller's configured relay hints. Do not merge relay addresses supplied
by the remote peer. An attacker otherwise could advertise its own address
as a relay and learn the caller's IP, despite direct transit being disabled.
No direct sockets, STUN or interface enumeration runs in this mode already.
The normal direct-capable mode retains upstream hint handling.

Keep this narrow patch until upstream offers a configured-relays-only policy.
Tests in `tests/transfer.py` exercise sandboxed transfers on private relays;
the adversarial relay test must keep the peer's canary address uncontacted.
