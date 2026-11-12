#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual module evaluation; runtime HTTP/ACME proof is a separate VM fixture."""
import argparse
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    fixture = Path(__file__).with_suffix(".nix")
    records = {}
    for variant in ("http01", "ipv4", "dns", "disabled", "ssl-conflict", "group-conflict", "no-acme"):
        negative = variant in {"ssl-conflict", "group-conflict", "no-acme"}
        apply = f'f: (f {{ variant = "{variant}"; }})' + (".failedAssertions" if negative else "")
        result = subprocess.run(["nix", "eval", "--offline", "--impure", "--json", "--file",
            str(fixture), "--apply", apply], capture_output=True, text=True, timeout=90)
        assert result.returncode == 0, f"{variant}: {result.stderr}"
        data = json.loads(result.stdout)
        (args.output / f"{variant}.json").write_text(json.dumps(data, indent=2) + "\n")
        if negative:
            expected = {"ssl-conflict": "dedicated HTTP-only", "group-conflict": "private nuschl-acme group",
                        "no-acme": "requires tls.enableACME"}[variant]
            assert any(expected in message for message in data), data
        else:
            assert not data["failedAssertions"], data["failedAssertions"]
            assert data["firewallTCP"] == []  # No implicit global exposure.
            assert "/run/nuschl-tls/current/fullchain.pem" in data["server"]["exec"]
            assert data["publisher"] == ["fullchain:/var/lib/acme/voice.example.test/fullchain.pem",
                                         "key:/var/lib/acme/voice.example.test/key.pem"]
            if variant == "disabled":
                assert not data["nginx"]["enable"]
                assert data["acme"]["voice.example.test"]["dnsProvider"] == "fixture-provider"
                assert data["acme"]["voice.example.test"]["webroot"] is None
                assert "nginx.service" not in data["certificateUnit"]["wants"]
            else:
                cert = data["acme"]["voice.example.test"]
                assert cert["group"] == "nuschl-acme" and cert["domain"] == "voice.example.test"
                assert cert["webroot"] == "/var/lib/nuschl-acme-challenge"
                assert all(cert[k] is None for k in ("dnsProvider", "listenHTTP", "s3Bucket"))
                assert "nuschl-acme" not in data["nginx"]["groups"]
                assert "nuschl-acme" not in data["server"]["SupplementaryGroups"]
                assert "-/var/lib/acme/voice.example.test" in data["nginx"]["inaccessible"]
                assert "-/run/nuschl-tls" in data["nginx"]["inaccessible"]
                assert "-/var/lib/acme/voice.example.test" in data["server"]["InaccessiblePaths"]
                assert "nginx.service" in data["certificateUnit"]["wants"]
                assert "nginx.service" in data["certificateUnit"]["after"]
                assert "acme-voice.example.test.service" not in data["nginx"]["after"]
                host = data["nginx"]["virtualHosts"]["voice.example.test"]
                assert all(not host[k] for k in ("enableACME", "addSSL", "forceSSL", "onlySSL", "rejectSSL"))
                assert host["useACMEHost"] is None and host["sslCertificate"] is None and host["sslCertificateKey"] is None
                assert [v["addr"] for v in host["listen"]] == (["0.0.0.0"] if variant == "ipv4" else ["0.0.0.0", "[::]"])
                assert all(v["port"] == 80 and not v["ssl"] for v in host["listen"])
                assert host["locations"]["/"]["return"] == "404"
                assert host["locations"]["^~ /.well-known/acme-challenge/"]["root"] == cert["webroot"]
                if variant == "dns":
                    assert data["acme"]["other.example.test"]["dnsProvider"] == "fixture-provider"
                    assert data["acme"]["other.example.test"]["group"] == "acme"
                else:
                    assert data["acme"]["web.example.test"]["domain"] == "web.example.test"
                    assert data["acme"]["witness.example.test"]["domain"] == "witness.example.test"
                    assert data["nginx"]["virtualHosts"]["web.example.test"]["enableACME"]
                    assert data["nginx"]["virtualHosts"]["witness.example.test"]["enableACME"]
        records[variant] = "passed"
    (args.output / "proof.json").write_text(json.dumps({"variants": records,
        "nixos_services_run": False, "acme_orders": False}, indent=2) + "\n")
    print("Seven real module-evaluation cases passed; HTTP/ACME runtime proof remains separate.")


if __name__ == "__main__":
    main()
