#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Repeated real native logins to owned loopback nuschld/stock Murmur only.

No GUI, audio, host profile, public server, or automatic credential retry. Every
attempt below is an explicit independent fixture connection. Unit tests cover
the deterministic reader/write ordering and no-Reject negative controls.
"""

import argparse
import collections
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

from local_murmur import Murmur, eventually, free_port
from local_nuschld import Nuschld
from mumble_client import Client, certificate_pin


class LoggedMurmur(Murmur):
    def start(self):
        # Foreground Murmur writes stderr instead of the ini logfile. Preserve
        # that diagnostic stream rather than masking a passed/failed assertion
        # with a missing logfile during cleanup.
        self.log = self.root / "murmur.log"
        with self.log.open("a") as log:
            self.process = subprocess.Popen(
                [str(self.binary), "-ini", str(self.ini), "-fg"],
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
            )
        eventually(self.listening, "stock Mumble server did not start")


def attempt(binaries, port, pin, profile, password):
    process = subprocess.Popen(
        [str(binaries / "nuschl-cli"), "--host", "127.0.0.1", "--port", str(port),
         "--username", "SuperUser", "--profile-dir", str(profile),
         "--certificate-sha256", pin, "--password-file", str(password), "--force-tcp"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        # Keep stdin OPEN until the server decides; communicate(input="") would
        # request Disconnect and race authentication instead of testing it.
        process.wait(timeout=15)
    except BaseException:
        process.kill()
        process.wait(timeout=5)
        raise
    finally:
        process.stdin.close()
    events = [json.loads(line) for line in process.stdout.read().splitlines()]
    stderr = process.stderr.read()
    process.stdout.close()
    process.stderr.close()
    assert not any(event["kind"] == "connected" for event in events), "wrong password accepted"
    terminal = [event for event in events if event["kind"] in {"rejected", "disconnected"}]
    assert len(terminal) == 1, events
    return {"exit_code": process.returncode, "event": terminal[0], "stderr": stderr[-2048:]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", required=True, type=Path, help="new native CLI bin directory")
    parser.add_argument("--server-binaries", required=True, type=Path, help="nuschld + adjacent storage helper")
    parser.add_argument("--murmur", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--attempts", type=int, default=40)
    args = parser.parse_args()
    assert 10 <= args.attempts <= 100
    args.output.mkdir(mode=0o700, parents=True, exist_ok=True)
    proof = {"passed": False, "loopback_only": True, "audio_opened": False,
             "cli": str(args.binaries / "nuschl-cli"),
             "cli_sha256": hashlib.sha256((args.binaries / "nuschl-cli").read_bytes()).hexdigest(),
             "servers": {}}
    try:
        with tempfile.TemporaryDirectory(prefix="private-login-", dir=args.output) as temporary:
            for name in ("nuschld", "murmur"):
                root = Path(temporary) / name
                root.mkdir()
                if name == "nuschld":
                    server = Nuschld(args.server_binaries, root, free_port())
                    server.password.chmod(0o600)
                else:
                    server = LoggedMurmur(args.murmur, root, free_port())
                    server.seed()
                server.start()
                try:
                    profile = root / "client-profile"
                    pin = certificate_pin(args.binaries, "127.0.0.1", server.port, profile)
                    good = root / "fixture-password"
                    good.write_text(server.SUPERUSER_PASSWORD)
                    good.chmod(0o600)
                    client = Client(args.binaries, "127.0.0.1", server.port, pin,
                                    "SuperUser", profile, ["--password-file", str(good), "--force-tcp"])
                    client.close()  # success control before intentional password-rate limiting
                    wrong = root / "wrong-fixture-password"
                    wrong.write_text("private-fixture-wrong-password")
                    wrong.chmod(0o600)
                    attempts = [attempt(args.binaries, server.port, pin, profile, wrong)
                                for _ in range(args.attempts)]
                    proof["servers"][name] = {
                        "successful_login_control": True, "attempts": attempts,
                        "counts": dict(collections.Counter(
                            entry["event"]["kind"] + ":" + entry["event"].get("reject", "")
                            for entry in attempts)),
                    }
                    assert all(entry["exit_code"] != 0 and entry["event"]["kind"] == "rejected"
                               for entry in attempts), proof["servers"][name]["counts"]
                    assert any(entry["event"].get("reject") == "wrong_user_password"
                               for entry in attempts), "missing actual wrong-password control"
                finally:
                    server.stop()
                    (args.output / (name + ".log")).write_text(server.log.read_text())
        proof["passed"] = True
    finally:
        (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps({name: data["counts"] for name, data in proof["servers"].items()}, indent=2))


if __name__ == "__main__":
    main()
