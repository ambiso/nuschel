#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Owned fixture preparation and guest-only systemd/TLS lifecycle assertions."""

import argparse
import grp
import hashlib
import json
import os
import pwd
import socket
import ssl
import stat
import struct
import subprocess
import time
from pathlib import Path


def run(*arguments, check=True):
    return subprocess.run(arguments, check=check, capture_output=True, text=True)


def wait(predicate, description, seconds=75):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, subprocess.CalledProcessError):
            pass
        time.sleep(0.1)
    raise AssertionError(f"Timed out: {description}")


def property_of(unit, name):
    return run(
        "systemctl", "show", "--property", name, "--value", "--", unit
    ).stdout.strip()


def prepare(directory):
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)
    assert not list(directory.iterdir()), "Use an empty owned fixture directory"
    run(
        "openssl",
        "req",
        "-x509",
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:P-256",
        "-nodes",
        "-days",
        "2",
        "-subj",
        "/CN=Nuschl private VM fixture root",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
        "-keyout",
        str(directory / "root.key"),
        "-out",
        str(directory / "root.pem"),
    )
    os.chmod(directory / "root.key", 0o600)


class Connection:
    def __init__(self, context):
        self.stream = context.wrap_socket(
            socket.create_connection(("127.0.0.1", 64738), timeout=3),
            server_hostname="voice.test",
        )
        self.stream.settimeout(5)

    def frame(self, kind, body):
        self.stream.sendall(struct.pack("!HI", kind, len(body)) + body)

    def exact(self, size):
        result = b""
        while len(result) < size:
            data = self.stream.recv(size - len(result))
            assert data, "Retained TLS stream closed"
            result += data
        return result

    def until(self, expected):
        for _ in range(100):
            kind, length = struct.unpack("!HI", self.exact(6))
            assert length <= 1024 * 1024
            body = self.exact(length)
            assert kind != 4, "Synthetic private login rejected"
            if kind == expected:
                return body
        raise AssertionError("Expected protocol response missing")

    def login(self):
        self.frame(0, b"\x08\x80\x8a\x04")
        username = b"private-vm-fixture"
        self.frame(2, b"\x0a" + bytes([len(username)]) + username + b"\x28\x01")
        self.until(5)

    def ping(self, number):
        payload = b"\x08" + bytes([number])
        self.frame(3, payload)
        assert self.until(3).startswith(payload)


def guest():
    assert os.geteuid() == 0
    assert Path("/etc/hostname").read_text().strip() == "server", "Guest only"
    directory = Path("/root/pki-fixture")
    source = Path("/var/lib/acme/voice.test")
    runtime = Path("/run/nuschl-tls")
    records = []
    proof = {"passed": False, "checks": records, "systemd_runtime_executed": True}
    context = ssl.create_default_context(cafile=str(directory / "root.pem"))

    def leaf(name, host="voice.test"):
        key, csr, certificate, extensions = [
            directory / f"{name}.{suffix}" for suffix in ("key", "csr", "pem", "ext")
        ]
        run(
            "openssl",
            "req",
            "-new",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
            "-subj",
            f"/CN={host}",
            "-keyout",
            str(key),
            "-out",
            str(csr),
        )
        extensions.write_text(
            f"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:{host}\n"
        )
        run(
            "openssl",
            "x509",
            "-req",
            "-in",
            str(csr),
            "-CA",
            str(directory / "root.pem"),
            "-CAkey",
            str(directory / "root.key"),
            "-set_serial",
            str(len(list(directory.glob("*.csr"))) + 10),
            "-days",
            "1",
            "-extfile",
            str(extensions),
            "-out",
            str(certificate),
        )
        return key, certificate

    def stage(pair):
        key, certificate = pair
        for name, contents in (
            ("key.pem", key.read_bytes()),
            (
                "fullchain.pem",
                certificate.read_bytes() + (directory / "root.pem").read_bytes(),
            ),
        ):
            destination = source / name
            destination.write_bytes(contents)
            os.chmod(destination, 0o600)
        return hashlib.sha256(
            ssl.PEM_cert_to_DER_cert(certificate.read_text())
        ).hexdigest()

    def fingerprint():
        connection = Connection(context)
        try:
            return hashlib.sha256(
                connection.stream.getpeercert(binary_form=True)
            ).hexdigest()
        finally:
            connection.stream.close()

    def renew():
        # The real NixOS post-run wrapper invokes our module's unchanged hook.
        run("systemctl", "start", "acme-order-renew-voice.test.service")

    try:
        run("systemctl", "start", "--no-block", "nuschl.service")
        for dependency in property_of("nuschl-tls-refresh.service", "Requires").split():
            assert property_of(dependency, "LoadState") != "not-found", (
                f"Missing helper dependency: {dependency}"
            )
        wait(
            lambda: int(property_of("nuschl.service", "NRestarts")) >= 1,
            "initial automatic retry",
        )
        assert property_of("nuschl.service", "MainPID") == "0"
        assert not Path("/var/lib/nuschl/server.sqlite3").exists()
        assert not (runtime / "current").exists()
        records.append(
            "untrusted bootstrap fails closed and retries without database preflight"
        )

        expected = stage(leaf("first"))
        renew()
        wait(
            lambda: property_of("nuschl.service", "ActiveState") == "active",
            "automatic startup after valid publication",
        )
        wait(lambda: fingerprint() == expected, "first verified TLS listener")
        pid = int(property_of("nuschl.service", "MainPID"))
        proof["pid"] = pid
        records.append("valid publication recovers failed startup automatically")

        status = dict(
            line.split(":", 1)
            for line in Path(f"/proc/{pid}/status").read_text().splitlines()
            if ":" in line
        )
        uid = int(status["Uid"].split()[0])
        groups = {int(value) for value in status["Groups"].split()}
        helper = pwd.getpwnam("nuschl-tls")
        assert uid not in (0, helper.pw_uid)
        assert helper.pw_gid in groups and grp.getgrnam("acme").gr_gid not in groups
        assert property_of("nuschl.service", "DynamicUser") == "yes"
        mount = [
            line.split()
            for line in Path(f"/proc/{pid}/mountinfo").read_text().splitlines()
            if line.split()[4] == "/run/nuschl-tls"
        ]
        assert mount and "ro" in mount[0][5].split(","), mount
        key = runtime / "current/key.pem"
        assert (
            key.stat().st_uid == helper.pw_uid
            and stat.S_IMODE(key.stat().st_mode) == 0o440
        )
        assert (
            run(
                "runuser",
                "-u",
                "nuschl-tls",
                "--",
                "test",
                "-r",
                str(source / "key.pem"),
                check=False,
            ).returncode
            != 0
        )
        records.append(
            "actual DynamicUser has selected group access and read-only runtime mount, without ACME key-directory access"
        )

        # Inspect the actual PID1-created, per-unit credential mount while the
        # test-only one-second pre-step holds the helper alive.
        run("systemctl", "start", "--no-block", "nuschl-tls-refresh.service")
        credential = Path("/run/credentials/nuschl-tls-refresh.service/key")
        wait(credential.exists, "helper credential mount", seconds=5)
        assert (
            run(
                "runuser", "-u", "nuschl-tls", "--", "test", "-r", str(credential)
            ).returncode
            == 0
        )
        assert (
            run(
                "setpriv",
                f"--reuid={uid}",
                f"--regid={helper.pw_gid}",
                f"--groups={helper.pw_gid}",
                "test",
                "-r",
                str(credential),
                check=False,
            ).returncode
            != 0
        )
        records.append("PID1 LoadCredential readable only in selected helper UID scope")
        wait(
            lambda: (
                property_of("nuschl-tls-refresh.service", "ActiveState") == "inactive"
            ),
            "helper completion",
            seconds=10,
        )

        retained = Connection(context)
        retained.login()
        retained.ping(1)
        expected = stage(leaf("renewed"))
        renew()
        wait(
            lambda: fingerprint() == expected,
            "renewed certificate after real privileged HUP",
        )
        assert int(property_of("nuschl.service", "MainPID")) == pid
        retained.ping(2)
        records.append(
            "valid renewal uses real fixed-unit privileged HUP, same PID and retained authenticated TLS stream"
        )

        previous = (runtime / "current").resolve()
        stage(leaf("wrong-host", host="wrong.test"))
        renew()
        wait(
            lambda: property_of("nuschl-tls-refresh.service", "Result") == "exit-code",
            "invalid renewal rejection",
            seconds=10,
        )
        assert (runtime / "current").resolve() == previous
        assert fingerprint() == expected
        assert int(property_of("nuschl.service", "MainPID")) == pid
        retained.ping(3)
        retained.stream.close()
        records.append(
            "invalid renewal retains published pair, active certificate, PID and TLS stream"
        )

        run("systemctl", "stop", "nuschl.service")
        expected = stage(leaf("after-stop"))
        renew()
        wait(
            lambda: (runtime / "current").resolve() != previous,
            "publication while server stopped",
            seconds=10,
        )
        time.sleep(35)
        assert property_of("nuschl.service", "ActiveState") == "inactive"
        assert property_of("nuschl.service", "MainPID") == "0"
        assert run("ss", "-ltn", "sport = :64738").stdout.count("\n") == 1
        records.append(
            "manual stop stays stopped across renewal hook, repeated timer refresh and restart backoff"
        )
        proof["passed"] = True
    except BaseException as error:
        proof["error"] = str(error)
        raise
    finally:
        (directory / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
        print(json.dumps(proof))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "guest"))
    parser.add_argument("--directory", type=Path)
    arguments = parser.parse_args()
    if arguments.mode == "prepare":
        assert arguments.directory is not None
        prepare(arguments.directory)
    else:
        guest()
