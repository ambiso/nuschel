#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Evaluate actual NixOS operator wiring, without building/running services."""
import argparse
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    args.output.mkdir(parents=True, exist_ok=True)
    observed = {}
    for variant in ("supplied", "complete", "resolved"):
        result = subprocess.run(["nix", "eval", "--offline", "--impure", "--json", "--file",
            str(root / "tests/server_operators.nix"), "--apply", f'f: f {{ variant = "{variant}"; }}'],
            capture_output=True, text=True, check=True, timeout=90)
        data = json.loads(result.stdout)
        (args.output / f"{variant}.json").write_text(json.dumps(data, indent=2) + "\n")
        assert not data["failedAssertions"], data["failedAssertions"]
        assert data["server"]["hostName"] == "nuschl.eu"
        assert data["server"]["security"] == "compatible"
        assert data["server"]["port"] == 64738 and not data["server"]["openFirewall"]
        assert data["server"]["database"]["type"] == "sqlite"
        assert data["server"]["database"]["sqlitePath"] == "/var/lib/nuschl/server.sqlite3"
        assert data["server"]["kt"]["origins"] == ["nuschl.eu:64738"]
        assert "superuser-password:/run/agenix/nuschl-superuser-password" in data["server"]["credentials"]
        assert "/run/nuschl-tls/current/fullchain.pem" in data["server"]["exec"]
        assert data["tls"]["credentials"] == [
            "fullchain:/var/lib/acme/nuschl.eu/fullchain.pem", "key:/var/lib/acme/nuschl.eu/key.pem"]
        assert data["tls"]["privateNetwork"]
        assert set(data["acme"]) == {"nuschl.eu", "witness.nuschl.eu"}
        assert all(v["webroot"] == "/var/lib/acme/acme-challenge" for v in data["acme"].values())
        assert data["web"]["enableACME"] and data["web"]["forceSSL"]
        assert data["web"]["redirect"] == "302 /app/"
        assert data["web"]["relayProxy"] == "http://unix:/run/nuschl-web-relay/relay.sock:"
        assert data["web"]["relay"]["allowNetworks"] == []
        assert data["web"]["relay"]["allowedOrigins"] == ["https://nuschl.eu"]
        assert data["web"]["relay"]["ports"] == ["64738-64799"]
        assert "10.0.0.0/8" in data["web"]["kernelDeny"]
        assert data["web"]["kernelAllow"] == (["127.0.0.53"] if variant == "resolved" else [])
        assert data["witness"]["name"] == "witness.nuschl.eu/nuschl-kt"
        assert data["witness"]["listenAddress"] == "127.0.0.1" and data["witness"]["port"] == 8473
        assert not data["witness"]["openFirewall"] and not data["witness"]["enrolPrivateAddresses"]
        assert not data["firewall"]["tcp"] and not data["firewall"]["udp"]
        assert data["firewall"]["tailscale0"]["allowedTCPPorts"] == [64738]
        assert set(data["firewall"]["ens3"]["allowedTCPPorts"]) == (
            {64738, 80, 443} if variant == "complete" else {64738})
        assert data["firewall"]["ens3"]["allowedUDPPorts"] == [64738]
        observed[variant] = "passed module evaluation; no runtime/deployment claim"
    (args.output / "proof.json").write_text(json.dumps({"variants": observed,
        "services_executed": False, "public_connections": False, "acme_orders": False,
        "fixture_verifier_key_is_not_validated": True}, indent=2) + "\n")
    print("Three actual NixOS module configurations passed; no services or network tests ran.")


if __name__ == "__main__":
    main()
