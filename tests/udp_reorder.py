#!/usr/bin/env python3
"""Deterministic UDP voice reordering between Nuschl clients and an owned server.

Run against a loopback server we start ourselves:
  with_local_murmur.py <mumble-server> udp_reorder.py <binaries> --port PORT --opus-library LIB
  with_local_nuschld.py <binaries> udp_reorder.py <binaries> --port PORT --opus-library LIB

Alice and Bob (the real session CLI) each reach the server through their own
in-process relay on one 127.0.0.1 port. It forwards TLS bytes unchanged and UDP
datagrams per direction. When armed, it delivers the next N voice-sized
(encrypted) datagrams of one direction in a fixed order; smaller datagrams
(pings) pass straight through. Patterns: in order, adjacent pairs swapped, a
reversed burst, one packet held back behind 20 later ones (inside the OCB2
nonce window: crypt.rs and stock CryptStateOCB2 accept a packet at most 29
nonces late) and behind 40 (outside it), and a bounded permutation from --seed.

Downstream phases reorder server -> Bob and check Nuschl's receiver: the order
of decrypted voice sequences, its OCB2 late/lost counters (transport stats), and
that a packet held beyond the window is dropped and counted as lost. Upstream
phases reorder Alice -> server and check the server's OCB2 counters (Alice's
own UserStats from_client) and that it relays in arrival order: Bob sees the
permuted sequences with no crypt-late packets. The expected counters follow
each implementation exactly: a late packet takes one off `lost`, but stock
CryptStateOCB2 only while its counter is above 1 (crypt.rs saturates at 0);
nuschld (Nuschl extension negotiated) counts like crypt.rs. Every delivered
Opus payload must match what was sent for its sequence, and the stream put
back in sequence order must decode (reference libopus) to the tone.

--negative-control keeps the relay in order with the same expectations: every
reordering phase must then fail on both order and counters, and the in-order
phase pass. The report (--output) is written for failed runs too.

The session CLI has no jitter buffer, so playout is not exercised here
(media_live.py feeds the media worker's buffer a reordered stream directly).
Synthetic Opus only; no audio devices.
"""
import argparse
import collections
import hashlib
import json
from pathlib import Path
import queue
import random
import signal
import socket
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, ReferenceOpus, certificate_pin, rms  # noqa: E402
from udp_voice import join, wait_transport  # noqa: E402

# Encrypted UDP pings are at most 16 bytes (4-byte OCB2 header, type byte and a
# varint timestamp); a voice datagram carries encode_tone's 60-byte CBR frame.
VOICE_MIN_BYTES = 40
# Accepted iff 0 < newest - nonce < 30 (crypt.rs, stock CryptStateOCB2).
NONCE_WINDOW = 30
MAX_PEERS = 4
MAX_CONNECTIONS = 8
TRACE = 4096


class Reorder:
    """One direction of the relay: the next len(order) voice datagrams leave in `order`."""

    def __init__(self, name):
        self.name = name
        self.lock = threading.Lock()
        self.order, self.held = [], {}
        self.arrived = self.delivered = 0
        self.counts = {"voice": 0, "small": 0, "unarmed_voice": 0, "events": 0, "arrival_gaps": 0, "send_errors": 0}
        self.voice_bytes = [None, None]
        # The OCB2 header's first byte is the nonce's low byte, in clear: arrivals
        # and departures by nonce show what the relay did and what the peer sent.
        self.trace = collections.deque(maxlen=TRACE)
        self.last_nonce = None

    def arm(self, order):
        assert sorted(order) == list(range(len(order))), order
        with self.lock:
            assert self.delivered == len(self.order) and not self.held, f"{self.name}: previous pattern pending"
            self.order, self.held, self.arrived, self.delivered = list(order), {}, 0, 0

    def _event(self, kind, datagram):
        self.counts["events"] += 1
        self.trace.append((self.counts["events"], kind, datagram[0] if datagram else None, len(datagram)))

    def _deliver(self, datagram, send):
        if not send(datagram):
            self.counts["send_errors"] += 1
        self._event("out", datagram)

    def push(self, datagram, send):
        with self.lock:
            self._event("in", datagram)
            if datagram and self.last_nonce is not None and (datagram[0] - self.last_nonce) % 256 != 1:
                self.counts["arrival_gaps"] += 1
            self.last_nonce = datagram[0] if datagram else self.last_nonce
            if len(datagram) < VOICE_MIN_BYTES:
                self.counts["small"] += 1
                self._deliver(datagram, send)
                return
            self.counts["voice"] += 1
            low, high = self.voice_bytes
            self.voice_bytes = [min(low or len(datagram), len(datagram)), max(high or 0, len(datagram))]
            if self.arrived >= len(self.order):
                self.counts["unarmed_voice"] += 1
                self._deliver(datagram, send)
                return
            self.held[self.arrived] = (datagram, send)
            self.arrived += 1
            while self.delivered < len(self.order) and self.order[self.delivered] in self.held:
                datagram, sender = self.held.pop(self.order[self.delivered])
                self._deliver(datagram, sender)
                self.delivered += 1

    def snapshot(self):
        with self.lock:
            return dict(self.counts)

    def trace_since(self, events):
        with self.lock:
            return [list(entry[1:]) for entry in self.trace if entry[0] > events]

    def finish(self):
        """End the pattern; anything still held (a datagram never arrived) leaves in index order."""
        with self.lock:
            state = {"armed": len(self.order), "arrived": self.arrived, "delivered": self.delivered,
                     "left_held": sorted(self.held)}
            for index in sorted(self.held):
                datagram, sender = self.held.pop(index)
                self._deliver(datagram, sender)
            self.order, self.arrived, self.delivered = [], 0, 0
            return state


class ReorderProxy:
    """TCP and UDP relay for one client on one 127.0.0.1 port (the session uses the TCP peer for UDP)."""

    def __init__(self, host, port):
        self.server = socket.getaddrinfo(host, port, type=socket.SOCK_DGRAM)[0]
        self.tcp_server = (host, port)
        self.upstream, self.downstream = Reorder("upstream"), Reorder("downstream")
        self.stopping = threading.Event()
        self.lock = threading.Lock()
        self.sockets, self.threads, self.peers = [], [], {}
        for _ in range(50):
            udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            udp.bind(("127.0.0.1", 0))
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            try:
                listener.bind(("127.0.0.1", udp.getsockname()[1]))
            except OSError:
                udp.close()
                listener.close()
                continue
            break
        else:
            raise AssertionError("no 127.0.0.1 port free for both TCP and UDP")
        listener.listen(MAX_CONNECTIONS)
        for owned in (udp, listener):
            owned.settimeout(0.2)
        self.udp, self.listener, self.port = udp, listener, udp.getsockname()[1]
        self.sockets += [udp, listener]
        self._spawn(self._accept)
        self._spawn(self._from_clients)

    def _spawn(self, target, *arguments):
        thread = threading.Thread(target=target, args=arguments, daemon=True)
        self.threads.append(thread)
        thread.start()

    def _accept(self):
        connections = 0
        while not self.stopping.is_set():
            try:
                client, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            connections += 1
            if connections > MAX_CONNECTIONS:
                client.close()
                continue
            try:
                server = socket.create_connection(self.tcp_server, timeout=5)
            except OSError:
                client.close()
                continue
            for owned in (client, server):
                owned.settimeout(None)
            with self.lock:
                self.sockets += [client, server]
            self._spawn(self._pump, client, server)
            self._spawn(self._pump, server, client)

    @staticmethod
    def _pump(source, target):
        try:
            while data := source.recv(65536):
                target.sendall(data)
        except OSError:
            pass
        finally:
            for owned in (source, target):
                try:
                    owned.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def _from_clients(self):
        while not self.stopping.is_set():
            try:
                datagram, address = self.udp.recvfrom(4096)
            except TimeoutError:
                continue
            except OSError:
                return
            upstream = self.peers.get(address)
            if upstream is None:
                if len(self.peers) >= MAX_PEERS:
                    continue
                family, kind, protocol, _, server = self.server
                upstream = socket.socket(family, kind, protocol)
                upstream.connect(server)
                upstream.settimeout(0.2)
                with self.lock:
                    self.sockets.append(upstream)
                self.peers[address] = upstream
                self._spawn(self._from_server, upstream, address)
            self.upstream.push(datagram, lambda data, upstream=upstream: self._send(upstream.send, data))

    def _from_server(self, upstream, address):
        while not self.stopping.is_set():
            try:
                datagram = upstream.recv(4096)
            except (TimeoutError, ConnectionRefusedError):
                continue
            except OSError:
                return
            self.downstream.push(datagram, lambda data: self._send(lambda d: self.udp.sendto(d, address), data))

    @staticmethod
    def _send(send, data):
        try:
            send(data)
            return True
        except OSError:
            return False

    def close(self):
        self.stopping.set()
        with self.lock:
            owned = list(self.sockets)
        for sock in owned:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            sock.close()
        for thread in self.threads:
            thread.join(timeout=2)


def pair_swap(count):
    return [index ^ 1 for index in range(count)]


def reversal(count, start, stop):
    order = list(range(count))
    order[start:stop] = reversed(order[start:stop])
    return order


def hold(count, index, behind):
    """Deliver `index` right after the `behind` packets that follow it."""
    order = [other for other in range(count) if other != index]
    order.insert(index + behind, index)
    return order


def seeded(count, seed, spread):
    """Bounded permutation: a packet is overtaken by fewer than `spread` later ones."""
    rng = random.Random(seed)
    keys = [index + rng.uniform(0, spread) for index in range(count)]
    return sorted(range(count), key=keys.__getitem__)


def late_distance(order):
    """The furthest a packet arrives behind the newest one delivered before it (in nonces, pings aside)."""
    newest, worst = -1, 0
    for index in order:
        worst, newest = max(worst, newest - index), max(newest, index)
    return worst


def ocb2_model(order, lost_before=0, stock=False):
    """Accepted packets in delivery order and the late/lost counter deltas.

    A late packet takes one off `lost`: crypt.rs saturates at zero, while stock
    CryptStateOCB2 subtracts only while uiLost > 1, so its counter never returns
    from 1 to 0. Interleaved pings pass unheld: they are never late, do not
    change the counter a late packet meets and only widen a gap, which every
    pattern here keeps well clear of the window edge.
    """
    accepted, late, lost, newest = [], 0, lost_before, -1
    for index in order:
        if index > newest:
            lost += index - newest - 1
            newest = index
        elif newest - index < NONCE_WINDOW:
            late += 1
            lost = lost - 1 if lost > (1 if stock else 0) else lost
        else:
            continue
        accepted.append(index)
    return accepted, late, lost - lost_before


def server_counters(client, override):
    """Which OCB2 counter rule the server follows, and the evidence for it."""
    negotiated = any(event["kind"] == "negotiated" and event.get("nuschl") for event in client.events)
    if override != "auto":
        assert (override == "nuschl") == negotiated, f"--server-counters {override}, but negotiated: {negotiated}"
        return override, f"--server-counters {override}"
    if negotiated:
        return "nuschl", "the Nuschl extension was negotiated: nuschld uses crypt.rs"
    return "stock", "no Nuschl extension negotiated: stock CryptStateOCB2"


def phases(seed):
    return [
        ("in-order", "down", list(range(20))),
        ("pair-swap", "down", pair_swap(20)),
        ("burst-reversal", "down", reversal(20, 6, 14)),
        ("hold-20-in-window", "down", hold(24, 2, 20)),
        ("seeded", "down", seeded(40, seed, 8)),
        ("hold-40-beyond-window", "down", hold(44, 2, 40)),
        ("upstream-pair-swap", "up", pair_swap(20)),
        ("upstream-hold-20-in-window", "up", hold(24, 2, 20)),
        ("upstream-seeded", "up", seeded(40, seed + 1, 8)),
        ("upstream-hold-40-beyond-window", "up", hold(44, 2, 40)),
    ]


def next_event(client, deadline):
    try:
        event = client.queue.get(timeout=max(0.01, deadline - time.monotonic()))
    except queue.Empty:
        return None
    if event is None:
        raise AssertionError(f"client exited: {list(client.events)[-8:]}")
    return event


def receive(bob, sender, base, count, expected, timeout=15):
    """Bob's audio from `sender` in arrival order, then the next transport stats after it."""
    arrivals, stray = [], 0

    def record(event):
        nonlocal stray
        if event["kind"] != "audio" or event["sender"] != sender:
            return
        if base <= event["sequence"] < base + 2 * count:
            arrivals.append((event["sequence"], event["opus"]))
        else:
            stray += 1

    deadline = time.monotonic() + timeout
    while len(arrivals) < expected and (event := next_event(bob, deadline)) is not None:
        record(event)
    # Transport events come every five seconds from the same loop that emits
    # audio: the next one counts every datagram decrypted before it.
    deadline = time.monotonic() + 12
    while (event := next_event(bob, deadline)) is not None:
        if event["kind"] == "transport":
            return arrivals, stray, event["stats"]
        record(event)
    raise AssertionError("no transport stats after the phase")


def server_view(alice):
    """The server's OCB2 counters for Alice's UDP (her own UserStats from_client)."""
    alice.send(kind="stats", session=alice.session)
    stats = alice.wait(lambda event: event["kind"] == "stats" and event["stats"]["session"] == alice.session)["stats"]
    assert stats.get("from_client"), stats
    return stats["from_client"]


def send_voice(alice, packets, base):
    start = time.monotonic()
    for index, packet in enumerate(packets):
        time.sleep(max(0, start + index * 0.02 - time.monotonic()))
        alice.send(kind="voice", sequence=base + 2 * index, opus=packet, end=index == len(packets) - 1)


def run_phase(number, phase, alice, bob, relays, tone, reference, negative, stock_server):
    name, direction, order = phase
    count = len(order)
    base = 1000 * (number + 1)
    packets = tone[:count]
    downstream = direction == "down"
    relay = relays["bob"].downstream if downstream else relays["alice"].upstream
    before_relay = relay.snapshot()
    before_bob = dict(bob.transport)
    before_server = server_view(alice)
    # Downstream, Nuschl's crypt.rs receives; upstream, the server's OCB2.
    accepted, late, lost = (ocb2_model(order, before_bob["lost"]) if downstream
                            else ocb2_model(order, before_server["lost"], stock_server))
    relay.arm(list(range(count)) if negative else order)
    send_voice(alice, packets, base)
    arrivals, stray, after_bob = receive(bob, alice.session, base, count, len(accepted))
    pattern = relay.finish()
    after_relay = relay.snapshot()
    after_server = server_view(alice)
    expected = {
        "arrival": [base + 2 * index for index in accepted],
        "receiver_late": late if downstream else 0,
        "receiver_lost": lost if downstream else 0,
        "server_late": 0 if downstream else late,
        "server_lost": 0 if downstream else lost,
        "relay_voice": count, "relay_unarmed_voice": 0, "relay_left_held": [], "relay_send_errors": 0, "stray": 0,
    }
    actual = {
        "arrival": [sequence for sequence, _ in arrivals],
        "receiver_late": after_bob["late"] - before_bob["late"],
        "receiver_lost": after_bob["lost"] - before_bob["lost"],
        "server_late": after_server["late"] - before_server["late"],
        "server_lost": after_server["lost"] - before_server["lost"],
        "relay_voice": after_relay["voice"] - before_relay["voice"],
        "relay_unarmed_voice": after_relay["unarmed_voice"] - before_relay["unarmed_voice"],
        "relay_left_held": pattern["left_held"], "stray": stray,
        "relay_send_errors": after_relay["send_errors"] - before_relay["send_errors"],
    }
    failures = [key for key in expected if expected[key] != actual[key]]
    # Every accepted packet was decrypted from UDP (pings add to `good` too).
    good = after_bob["good"] - before_bob["good"]
    if good < len(accepted) or not after_bob["udp_active"]:
        failures.append("receiver_udp")
    sent = {base + 2 * index: packet for index, packet in enumerate(packets)}
    if any(sent.get(sequence) != opus for sequence, opus in arrivals):
        failures.append("payload")
    level = None
    if arrivals:
        level = rms(reference.decode([opus for _, opus in sorted(arrivals)]))
        if not 0.02 < level < 0.4:
            failures.append("decoded_level")
    missing = sorted(set(range(base, base + 2 * count, 2)) - {sequence for sequence, _ in arrivals})
    result = {"phase": name, "direction": direction, "base": base, "order": order,
              "late_distance": late_distance(order),
              "expected": expected, "actual": actual, "receiver_good": good, "missing": missing,
              "decoded_rms": level, "relay_pattern": pattern, "relay_counts": [before_relay, after_relay],
              "receiver_stats": [before_bob, after_bob], "server_from_client": [before_server, after_server],
              "relay_trace": relay.trace_since(before_relay["events"]),
              "failures": failures}
    status = "ok" if not failures else f"FAILED {failures}"
    print(f"{name}: {len(arrivals)}/{count} delivered, missing {missing}, late/lost at receiver "
          f"{actual['receiver_late']}/{actual['receiver_lost']}, at server {actual['server_late']}/"
          f"{actual['server_lost']}: {status}", flush=True)
    return result


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1", help="the owned server the relays forward to")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=107, help="seed of the bounded random patterns")
    parser.add_argument("--server-counters", choices=("auto", "stock", "nuschl"), default="auto",
                        help="the server's OCB2 counter rule; auto: nuschl iff the Nuschl extension was negotiated")
    parser.add_argument("--negative-control", action="store_true",
                        help="relay in order with the same expectations: reordering phases must fail")
    parser.add_argument("--output", type=Path, help="write udp_reorder.json here")
    parser.add_argument("--timeout", type=int, default=300, help="wall-clock limit in seconds")
    args = parser.parse_args()

    def expired(*_):
        raise TimeoutError(f"udp_reorder.py exceeded {args.timeout} s")
    signal.signal(signal.SIGALRM, expired)
    signal.alarm(args.timeout)

    plan = phases(args.seed)
    for name, _, order in plan:
        # Pings may add a nonce or two to a gap: in-window patterns stay far from 30.
        distance = late_distance(order)
        assert ("beyond-window" in name) == (distance >= NONCE_WINDOW), (name, distance)
        assert "beyond-window" in name or distance <= 20, (name, distance)
    pattern_digest = hashlib.sha256(json.dumps([list(phase) for phase in plan]).encode()).hexdigest()
    print(f"Seed {args.seed}, pattern sha256 {pattern_digest}"
          + (", negative control" if args.negative_control else ""), flush=True)
    reference = ReferenceOpus(args.opus_library)
    tone = reference.encode_tone(max(len(order) for _, _, order in plan))
    # Filled in as the run goes, so a failed run still leaves its report.
    run_state = {"results": [], "relays": {}, "counters": None, "evidence": None, "error": None}
    results, relays = run_state["results"], run_state["relays"]
    try:
        run(args, plan, tone, reference, run_state)
    except BaseException as exception:
        run_state["error"] = repr(exception)
        raise
    finally:
        report = {
            "seed": args.seed, "pattern_sha256": pattern_digest, "negative_control": args.negative_control,
            "server_counters": run_state["counters"], "server_counters_evidence": run_state["evidence"],
            "error": run_state["error"],
            "server": f"{args.host}:{args.port}", "binaries": str(args.binaries.resolve()),
            "nuschl_cli_sha256": sha256(args.binaries / "nuschl-cli"),
            "opus_library": str(args.opus_library.resolve()), "opus_bytes": sorted({len(packet) for packet in tone}),
            "voice_datagram_bytes": {name: {"up": relay.upstream.voice_bytes, "down": relay.downstream.voice_bytes}
                                     for name, relay in relays.items()},
            "relay_counts": {name: {"up": relay.upstream.snapshot(), "down": relay.downstream.snapshot()}
                             for name, relay in relays.items()},
            "nonce_window": NONCE_WINDOW, "voice_min_bytes": VOICE_MIN_BYTES, "phases": results,
        }
        if args.output:
            args.output.mkdir(parents=True, exist_ok=True)
            (args.output / "udp_reorder.json").write_text(json.dumps(report, indent=1) + "\n")
    failed = [result["phase"] for result in results if result["failures"]]
    if args.negative_control:
        reordering = [result for result in results if result["phase"] != "in-order"]
        counter_keys = {"receiver_late", "receiver_lost", "server_late", "server_lost"}
        undetected = [result["phase"] for result in reordering
                      if "arrival" not in result["failures"] or not counter_keys & set(result["failures"])]
        assert "in-order" not in failed, "the in-order phase failed without reordering"
        assert not undetected, f"reordering expectations held without reordering: {undetected}"
        print(f"Negative control ok: all {len(reordering)} reordering phases failed on order and counters "
              "with the relay in order.")
    else:
        assert not failed, f"phases failed: {failed}"
        print(f"UDP reordering ok: {len(results)} phases, seed {args.seed}.")


def run(args, plan, tone, reference, run_state):
    relays, clients = run_state["relays"], []
    with tempfile.TemporaryDirectory(prefix="nuschl-reorder-") as temporary:
        root = Path(temporary)
        try:
            pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
            relays.update((name, ReorderProxy(args.host, args.port)) for name in ("alice", "bob"))
            alice = Client(args.binaries, "127.0.0.1", relays["alice"].port, pin, "ReorderAlice", root / "alice")
            clients.append(alice)
            bob = Client(args.binaries, "127.0.0.1", relays["bob"].port, pin, "ReorderBob", root / "bob")
            clients.append(bob)
            for client in clients:
                join(client, 1)
            for client in clients:
                wait_transport(client, lambda stats: stats["udp_active"], f"{client} never activated UDP")
            counters, evidence = server_counters(alice, args.server_counters)
            run_state |= {"counters": counters, "evidence": evidence}
            print(f"Server OCB2 counters: {counters} ({evidence})", flush=True)
            alice.unmute()
            for number, phase in enumerate(plan):
                run_state["results"].append(run_phase(number, phase, alice, bob, relays, tone, reference,
                                                      args.negative_control, counters == "stock"))
            for client in clients:
                assert client.process.poll() is None, f"{client} exited during the run"
        finally:
            for client in clients:
                client.close()
            for relay in relays.values():
                relay.close()


if __name__ == "__main__":
    main()
