#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Guest-only real HTTP01/renewal/access assertions; never reads a private key."""
import argparse
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shlex
import ssl
import subprocess
import time
import urllib.request


def run(*args, check=True, input=None):
    result = subprocess.run(args, input=input, capture_output=True)
    if check and result.returncode:
        raise AssertionError(f"{args[:3]} exit {result.returncode}: " + result.stderr.decode(errors="replace")[:8192])
    return result


def property_of(unit, name):
    return run("systemctl", "show", "--property", name, "--value", unit).stdout.decode().strip()


def wait(predicate, description, seconds=60):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, subprocess.CalledProcessError):
            pass
        time.sleep(.1)
    raise AssertionError("Timed out: " + description)


def can_read(unit, path, namespace=True):
    pid = int(property_of(unit, "MainPID"));assert pid > 1
    status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines() if ":" in line)
    uid, gid = status["Uid"].split()[3], status["Gid"].split()[3]
    groups = status["Groups"].split()
    args = ["setpriv", "--reuid", uid, "--regid", gid,
            *( ["--groups", ",".join(groups)] if groups else ["--clear-groups"] ),
            "--bounding-set=-all", "--inh-caps=-all", "--ambient-caps=-all", "--no-new-privs",
            "/bin/sh", "-c", 'test -r "$1"', "read-probe", path]
    if namespace:
        args = ["nsenter", "--target", str(pid), "--mount", "--", *args]
    result = run(*args, check=False)
    assert result.returncode in (0, 1), result.stderr.decode()
    return result.returncode == 0


def request_http(host, path, method="GET"):
    connection = http.client.HTTPConnection("127.0.0.1", 80, timeout=5)
    try:
        connection.request(method, path, headers={"Host": host})
        response = connection.getresponse()
        body = response.read(65537);assert len(body) <= 65536
        return response.status, body
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pki-helper", required=True, type=Path)
    args = parser.parse_args()
    assert os.geteuid() == 0 and Path("/etc/hostname").read_text().strip() == "server", "VM guest only"
    spec = importlib.util.spec_from_file_location("pki_protocol", args.pki_helper)
    pki = importlib.util.module_from_spec(spec);spec.loader.exec_module(pki)
    directory = Path("/run/http01-test");directory.mkdir(mode=0o700, exist_ok=True)
    cfg = json.loads(Path("/etc/http01-test-config.json").read_text())
    proof = {"passed": False, "real_lego_acme": True, "issuer": "private Pebble 2.6.0",
             "validation_not_skipped": True, "public_connections": False,
             "test_only_publisher_root_bind": True, "checks": []}
    records = proof["checks"]
    retained = None
    ca_before = hashlib.sha256(Path(cfg["caBundle"]).read_bytes()).hexdigest()
    try:
        frontend = ssl.create_default_context(cafile=cfg["frontendCA"])
        with urllib.request.urlopen("https://acme.test:15000/roots/0", context=frontend, timeout=5) as response:
            root = response.read(65537);assert len(root) <= 65536 and response.status == 200
        assert root.startswith(b"-----BEGIN CERTIFICATE-----")
        (directory / "issuer.pem").write_bytes(root)
        context = ssl.create_default_context(cafile=str(directory / "issuer.pem"))
        records.append("ephemeral issuer public root fetched over authenticated private management TLS")
        for name in ("web.test", "witness.test", "voice.test"):
            run("systemctl", "start", f"acme-order-renew-{name}.service")
            assert property_of(f"acme-order-renew-{name}.service", "Result") == "success"
            # Separate VM clocks can differ by a second just after issuance.
            # Wait only for this public leaf's real validity start; retain
            # ordinary OpenSSL time/chain verification and bound clock skew.
            start = run("openssl", "x509", "-in", f"/var/lib/acme/{name}/cert.pem",
                        "-noout", "-startdate").stdout.decode().strip()
            remaining = ssl.cert_time_to_seconds(start.removeprefix("notBefore=")) - time.time()
            assert remaining <= 10, f"issuer/client clock skew exceeds fixture budget: {remaining}s"
            if remaining > 0:
                time.sleep(remaining + .05)
            run("openssl", "verify", "-CAfile", str(directory / "issuer.pem"), "-untrusted",
                f"/var/lib/acme/{name}/fullchain.pem", f"/var/lib/acme/{name}/cert.pem")
        records.append("real lego HTTP01 issuance for separate voice/web/witness certificates")

        public_keys = {}
        for name in ("voice.test", "web.test", "witness.test"):
            certificate = f"/var/lib/acme/{name}/cert.pem"
            run("openssl", "x509", "-in", certificate, "-noout", "-checkhost", name)
            for other in {"voice.test", "web.test", "witness.test"} - {name}:
                assert run("openssl", "x509", "-in", certificate, "-noout", "-checkhost", other,
                           check=False).returncode != 0
            pem = run("openssl", "x509", "-in", certificate, "-pubkey", "-noout").stdout
            der = run("openssl", "pkey", "-pubin", "-outform", "DER", input=pem).stdout
            public_keys[name] = hashlib.sha256(der).hexdigest()
        assert len(set(public_keys.values())) == 3
        proof["public_key_sha256"] = public_keys
        records.append("three distinct certificate public keys and disjoint requested hostnames")

        token = Path("/var/lib/nuschl-acme-challenge/.well-known/acme-challenge/owned-fixture-token")
        token.write_text("owned challenge response\n");token.chmod(0o644)
        assert request_http("voice.test", "/.well-known/acme-challenge/owned-fixture-token") == (200, b"owned challenge response\n")
        assert request_http("voice.test", "/.well-known/acme-challenge/missing")[0] == 404
        assert request_http("voice.test", "/.well-known/acme-challenge/owned-fixture-token", "POST")[0] == 403
        for path in ("/", "/app/", "/fullchain.pem", "/key.pem", "/any/other/path?x=1"):
            assert request_http("voice.test", path)[0] == 404
        token.unlink()
        records.append("actual challenge GET succeeds; other HTTP paths/missing tokens/POST refused")

        rendered = run(*shlex.split(cfg["nginxCommand"]), "-T").stdout.decode()
        assert "server_name voice.test" in rendered
        assert "/var/lib/acme/voice.test/" not in rendered
        assert "/run/nuschl-tls/" not in rendered
        # Actual certificate/SNI behavior supplements the source listener check.
        for name in ("web.test", "witness.test"):
            with context.wrap_socket(__import__("socket").create_connection(("127.0.0.1", 443), timeout=5),
                                     server_hostname=name) as stream:
                assert stream.getpeercert()
        try:
            with context.wrap_socket(__import__("socket").create_connection(("127.0.0.1", 443), timeout=5),
                                     server_hostname="voice.test"):
                raise AssertionError("voice HTTPS unexpectedly accepted")
        except ssl.SSLCertVerificationError as error:
            assert error.verify_code == 62, error  # host mismatch, not unknown issuer
        records.append("active nginx config contains no voice key/chain reference; cohost HTTPS works and voice HTTPS fails hostname validation")

        run("systemctl", "start", "nuschl-tls-refresh.service")
        run("systemctl", "start", "nuschl.service")
        retained = wait(lambda: pki.Connection(context), "native verified TLS")
        retained.login();retained.ping(1)
        old_certificate = hashlib.sha256(retained.stream.getpeercert(binary_form=True)).hexdigest()
        pid = property_of("nuschl", "MainPID")
        records.append("native server starts with the selected private CA-issued voice chain")
        for namespace in (False, True):
            assert can_read("nginx", "/var/lib/acme/web.test/key.pem", namespace)
            assert can_read("nginx", "/var/lib/acme/witness.test/key.pem", namespace)
            assert not can_read("nginx", "/var/lib/acme/voice.test/key.pem", namespace)
            assert not can_read("nginx", "/run/nuschl-tls/current/key.pem", namespace)
            assert not can_read("nuschl", "/var/lib/acme/voice.test/key.pem", namespace)
            assert can_read("nuschl", "/run/nuschl-tls/current/key.pem", namespace)
        records.append("actual service UIDs/groups plus mount namespaces: positive web-key/runtime controls pass; nginx cannot read voice source/runtime key; server cannot read source key")

        access_before = Path("/var/log/nginx/access.log").read_text().count("/.well-known/acme-challenge/")
        run("systemctl", "start", "acme-order-renew-voice.test.service")
        def changed():
            connection = pki.Connection(context)
            try:
                return hashlib.sha256(connection.stream.getpeercert(binary_form=True)).hexdigest() != old_certificate
            finally:
                connection.stream.close()
        wait(changed, "new voice certificate published/reloaded")
        assert property_of("nuschl", "MainPID") == pid
        retained.ping(2)
        assert Path("/var/log/nginx/access.log").read_text().count("/.well-known/acme-challenge/") > access_before
        records.append("real ACME renewal performs HTTP validation, publishes/reloads without server restart, retained authenticated client still pings")
        assert hashlib.sha256(Path(cfg["caBundle"]).read_bytes()).hexdigest() == ca_before
        proof["outside_publisher_ca_bundle_unchanged"] = True
        proof["publisher_script_sha256"] = hashlib.sha256(Path(cfg["publisherCommand"]).read_bytes()).hexdigest()
        proof["server_executable"] = os.readlink(f"/proc/{pid}/exe")
        proof["passed"] = True
    except BaseException as error:
        proof["error"] = str(error)
        raise
    finally:
        if retained:
            retained.stream.close()
        # Only PUBLIC certificates, never keys, are retained for failure diagnosis.
        for name, source in [("issuer", directory / "issuer.pem"), *(
            (host + "-" + kind, Path(f"/var/lib/acme/{host}/{kind}.pem"))
            for host in ("voice.test", "web.test", "witness.test")
            for kind in ("cert", "fullchain"))]:
            if source.exists():
                (directory / ("public-" + name + ".pem")).write_bytes(source.read_bytes())
        (directory / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps(proof, indent=2))


if __name__ == "__main__":
    main()
