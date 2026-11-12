#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Private PKI/module/helper tests. No ACME network calls or host configuration.

Generate tiny disposable keys on disk; only the PUBLIC fixture root certificate
is an input to Nix. Run with disk-backed TMPDIR and --output for evidence.
"""

import argparse
import contextlib
import json
import os
import shutil
import signal
import socket
import ssl
import struct
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(command, **kwargs):
    return subprocess.run(command, check=True, capture_output=True, text=True, **kwargs)


class LiveServer(contextlib.AbstractContextManager):
    """Owned process only: no systemd operations or host configuration."""

    def __init__(self, binaries, directory, runtime, root_cert):
        self.process = None
        self.connection = None
        self.log = directory / "server.log"
        self.context = ssl.create_default_context(cafile=str(root_cert))
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        config = directory / "server.toml"
        config.write_text(
            f'bind=["127.0.0.1"]\nport={self.port}\nsecurity="compatible"\nstate_directory="{directory / "server-state"}"\n[kt]\ndefault_witnesses=false\norigins=[]\nwitnesses=[]\n'
        )
        self.process = subprocess.Popen(
            [
                str(binaries / "nuschld"),
                "--config",
                str(config),
                "--tls-certificate-file",
                str(runtime / "current/fullchain.pem"),
                "--tls-key-file",
                str(runtime / "current/key.pem"),
            ],
            stdout=self.log.open("w"),
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            for _ in range(200):
                if self.process.poll() is not None:
                    raise AssertionError(self.log.read_text()[-3000:])
                try:
                    self.connection = self.connect()
                    break
                except OSError:
                    time.sleep(0.05)
            assert self.connection is not None, "private server startup timed out"
            self.connection.sendall(self.frame(0, b"\x08" + self.varint(0x10500)))
            user = b"private-pki-fixture"
            self.connection.sendall(
                self.frame(2, b"\x0a" + self.varint(len(user)) + user + b"\x28\x01")
            )
            self.until(5)
            self.ping(1)
        except BaseException:
            self.__exit__(None, None, None)
            raise

    @staticmethod
    def varint(value):
        out = bytearray()
        while value > 127:
            out.append((value & 127) | 128)
            value >>= 7
        out.append(value)
        return bytes(out)

    @staticmethod
    def frame(kind, body):
        return struct.pack("!HI", kind, len(body)) + body

    def connect(self):
        stream = self.context.wrap_socket(
            socket.create_connection(("127.0.0.1", self.port), timeout=3),
            server_hostname="voice.test",
        )
        stream.settimeout(5)
        return stream

    def exact(self, length):
        data = b""
        while len(data) < length:
            chunk = self.connection.recv(length - len(data))
            assert chunk, "private connection ended"
            data += chunk
        return data

    def until(self, expected):
        for _ in range(100):
            kind, length = struct.unpack("!HI", self.exact(6))
            assert length <= 1024 * 1024
            body = self.exact(length)
            assert kind != 4, "private synthetic login was refused"
            if kind == expected:
                return body
        raise AssertionError("private protocol response missing")

    def ping(self, number):
        payload = b"\x08" + self.varint(number)
        self.connection.sendall(self.frame(3, payload))
        assert self.until(3).startswith(payload)

    def fingerprint(self):
        with self.connect() as connection:
            return connection.getpeercert(binary_form=True)

    def reload(self, expected):
        assert self.process.poll() is None
        self.process.send_signal(signal.SIGHUP)
        for _ in range(100):
            if self.fingerprint() == expected:
                break
            time.sleep(0.02)
        assert self.fingerprint() == expected, "new certificate not loaded after HUP"
        self.ping(2)

    def __exit__(self, *_):
        if self.connection:
            self.connection.close()
        if self.process and self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument(
        "--binaries",
        type=Path,
        help="optional current package/bin for actual private nuschld HUP/same-process proof",
    )
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    openssl = shutil.which("openssl")
    assert openssl, "cached openssl is required"
    records = []
    live = None
    with (
        tempfile.TemporaryDirectory(prefix="server-private-pki-") as temporary,
        contextlib.ExitStack() as stack,
    ):
        directory = Path(temporary)
        os.chmod(directory, 0o700)
        root_key, root_cert = directory / "root.key", directory / "root.pem"
        run(
            [
                openssl,
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
                "/CN=Nuschl disposable root",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
                "-keyout",
                str(root_key),
                "-out",
                str(root_cert),
            ]
        )
        intermediate_key, intermediate_cert = (
            directory / "intermediate.key",
            directory / "intermediate.pem",
        )
        csr, extensions = directory / "intermediate.csr", directory / "intermediate.ext"
        run(
            [
                openssl,
                "req",
                "-new",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-subj",
                "/CN=Nuschl disposable intermediate",
                "-keyout",
                str(intermediate_key),
                "-out",
                str(csr),
            ]
        )
        extensions.write_text(
            "basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\n"
        )
        run(
            [
                openssl,
                "x509",
                "-req",
                "-in",
                str(csr),
                "-CA",
                str(root_cert),
                "-CAkey",
                str(root_key),
                "-set_serial",
                "2",
                "-days",
                "2",
                "-extfile",
                str(extensions),
                "-out",
                str(intermediate_cert),
            ]
        )

        def leaf(name, host="voice.test", days="1"):
            key, cert, csr, ext = [
                directory / f"{name}.{suffix}"
                for suffix in ("key", "pem", "csr", "ext")
            ]
            run(
                [
                    openssl,
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
                ]
            )
            ext.write_text(
                f"basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:{host}\nextendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature\n"
            )
            validity = (
                ["-days", days]
                if days != "-1"
                else ["-not_before", "20000101000000Z", "-not_after", "20010101000000Z"]
            )
            run(
                [
                    openssl,
                    "x509",
                    "-req",
                    "-in",
                    str(csr),
                    "-CA",
                    str(intermediate_cert),
                    "-CAkey",
                    str(intermediate_key),
                    "-set_serial",
                    str(10 + len(records)),
                    *validity,
                    "-extfile",
                    str(ext),
                    "-out",
                    str(cert),
                ]
            )
            return key, cert

        def nix_args(variant="acme"):
            return [
                "--impure",
                "--file",
                str(ROOT / "tests/server_pki.nix"),
                "--argstr",
                "trustedCaFile",
                str(root_cert),
                "--argstr",
                "variant",
                variant,
            ]

        states = {}
        for variant in [
            "acme",
            "reuse",
            "manual",
            "self-signed",
            "missing-host",
            "conflict",
            "store-key",
        ]:
            state = json.loads(
                run(["nix", "eval", *nix_args(variant), "--json", "state"]).stdout
            )
            states[variant] = state
            failures = state["failedAssertions"]
            assert bool(failures) == (
                variant in ["missing-host", "conflict", "store-key"]
            ), (variant, failures)
        main = states["acme"]["server"]
        helper = states["acme"]["helper"]
        assert main["SupplementaryGroups"] == ["nuschl-tls"]
        assert main["ReadOnlyPaths"] == ["/run/nuschl-tls"]
        assert all("tls-" not in item for item in main["LoadCredential"])
        assert helper["User"] == helper["Group"] == "nuschl-tls"
        assert helper["PrivateNetwork"] and helper["ProtectHome"]
        assert helper["LoadCredential"] == [
            "fullchain:/var/lib/acme/voice.test/fullchain.pem",
            "key:/var/lib/acme/voice.test/key.pem",
        ]
        assert (
            "acme-finished-voice.test.target" not in states["acme"]["after"]
        )  # helper orders provisioning
        assert "nuschl-tls-refresh.service" in states["acme"]["after"]
        assert "nuschl-tls-refresh.service" in states["acme"]["wants"]
        assert "nuschl-tls-refresh.service" not in states["acme"]["requires"]
        assert states["acme"]["unit"]["StartLimitIntervalSec"] == 0
        assert main["Restart"] == "on-failure" and main["RestartSec"] == "30s"
        assert len(main["ExecStartPre"]) == 2
        assert "nuschl-require-published-tls" in main["ExecStartPre"][0]
        assert "--check-database" in main["ExecStartPre"][1]
        for variant in ["manual", "self-signed"]:
            assert states[variant]["server"]["RestartSec"] == "5s"
            assert len(states[variant]["server"]["ExecStartPre"]) == 1
        assert (
            "/var/lib/acme/shared.test/"
            in states["reuse"]["helper"]["LoadCredential"][0]
        )
        assert (
            "tls-key:/private/key.pem" in states["manual"]["server"]["LoadCredential"]
        )
        assert states["self-signed"]["helper"] is None
        records.append(
            "module options/order/scoped credentials/invalid-config assertions"
        )
        build = run(
            [
                "nix",
                "build",
                *nix_args(),
                "helperScript",
                "--cores",
                "2",
                "--max-jobs",
                "2",
                "--no-link",
                "--print-out-paths",
            ]
        )
        (args.output / "helper-build.log").write_text(build.stderr)
        script = Path(build.stdout.strip()) / "refresh"
        startup_check = script.parent / "require-runtime"
        assert "PRIVATE KEY" not in script.read_text()
        credentials, runtime = directory / "credentials", directory / "runtime"
        credentials.mkdir(mode=0o700)
        runtime.mkdir(mode=0o750)
        env = {
            **os.environ,
            "CREDENTIALS_DIRECTORY": str(credentials),
            "RUNTIME_DIRECTORY": str(runtime),
        }

        def ready(expected):
            result = subprocess.run(
                [str(startup_check), str(runtime)],
                check=False,
                capture_output=True,
                text=True,
            )
            assert (result.returncode == 0) == expected, result.stderr
            assert "PRIVATE KEY" not in result.stdout + result.stderr

        ready(False)
        # A partial pair cannot get as far as database checks or server start.
        partial = runtime / "current"
        partial.mkdir()
        (partial / "fullchain.pem").write_text("not a published pair")
        ready(False)
        shutil.rmtree(partial)
        records.append(
            "startup gate rejects missing or partial runtime pair before database checks"
        )

        def publish(key, cert, *, chain=True, expected=True):
            (credentials / "key").write_bytes(key.read_bytes())
            (credentials / "fullchain").write_bytes(
                cert.read_bytes() + (intermediate_cert.read_bytes() if chain else b"")
            )
            result = subprocess.run(
                [str(script)], check=False, env=env, capture_output=True, text=True
            )
            assert (result.returncode == 0) == expected, result.stderr
            assert "PRIVATE KEY" not in result.stdout + result.stderr
            return result

        first_key, first_cert = leaf("first")
        publish(first_key, first_cert)
        ready(True)
        first = (runtime / "current").resolve()
        assert (first / "key.pem").stat().st_mode & 0o777 == 0o440
        assert first.stat().st_mode & 0o777 == 0o750
        records.append(
            "private fullchain published with read-only key/group permissions"
        )
        if args.binaries:
            live = stack.enter_context(
                LiveServer(args.binaries, directory, runtime, root_cert)
            )
            first_pid = live.process.pid
            assert live.fingerprint() == ssl.PEM_cert_to_DER_cert(
                first_cert.read_text()
            )
        second_key, second_cert = leaf("renewed")
        publish(second_key, second_cert)
        second = (runtime / "current").resolve()
        assert first != second and not first.exists()
        assert (
            (second / "fullchain.pem").read_bytes().startswith(second_cert.read_bytes())
        )
        records.append(
            "valid renewed pair atomically replaces prior runtime generation"
        )
        if live:
            live.reload(ssl.PEM_cert_to_DER_cert(second_cert.read_text()))
            assert live.process.pid == first_pid
            records.append(
                "actual nuschld HUP: new fullchain, unchanged PID, prior authenticated stream still answers"
            )
        for label, key, cert, chain in [
            ("mismatching key", first_key, second_cert, True),
            ("missing intermediate", second_key, second_cert, False),
            ("wrong hostname", *leaf("wrong", "other.test"), True),
            ("expired", *leaf("expired", days="-1"), True),
        ]:
            publish(key, cert, chain=chain, expected=False)
            ready(True)
            assert (runtime / "current").resolve() == second
            assert (second / "key.pem").read_bytes() == second_key.read_bytes()
            assert not list(runtime.glob(".staging.*"))
            if live:
                live.ping(3)
                assert live.fingerprint() == ssl.PEM_cert_to_DER_cert(
                    second_cert.read_text()
                )
                assert live.process.pid == first_pid
            records.append(f"{label}: previous valid generation retained")
        if args.binaries:
            restart_directory = directory / "restart-with-retained-pair"
            restart_directory.mkdir(mode=0o700)
            with LiveServer(
                args.binaries, restart_directory, runtime, root_cert
            ) as restarted:
                assert restarted.fingerprint() == ssl.PEM_cert_to_DER_cert(
                    second_cert.read_text()
                )
            records.append(
                "actual server starts with retained validated pair after source validation fails"
            )
        malformed_key = directory / "malformed.key"
        malformed_key.write_text("not a private key\n")
        publish(malformed_key, second_cert, expected=False)
        assert (runtime / "current").resolve() == second
        oversized_key = directory / "oversized.key"
        oversized_key.write_bytes(b"x" * 65537)
        publish(oversized_key, second_cert, expected=False)
        assert (runtime / "current").resolve() == second
        records.append(
            "malformed and oversized key: current generation unchanged, private bytes not logged"
        )
        # Interrupted helper state is owned/limited; it cannot wedge publication.
        (runtime / ".next").symlink_to("missing")
        orphan = runtime / "generation.orphan"
        orphan.mkdir()
        publish(second_key, second_cert)
        assert not orphan.exists()
        assert len(list(runtime.glob("generation.*"))) == 1
        records.append(
            "stale staging/link cleanup is bounded to owned runtime directory"
        )
        (args.output / "module-state.json").write_text(json.dumps(states, indent=2))
    report = {
        "passed": True,
        "checks": records,
        "public_network": False,
        "host_config_changed": False,
        "private_keys_in_store": False,
        "systemd_runtime_executed": False,
        "actual_nuschld_hup": args.binaries is not None,
        "binaries": str(args.binaries) if args.binaries else None,
        "limits": "Module eval and exact helper script; service lifecycle/UID mounts require VM proof.",
    }
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
