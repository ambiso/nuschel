#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Offline, reviewable renewal of authenticated v2 metadata, never publication.

Only the two top-level time integers change. No artifact is fetched/repacked,
no key is generated/enrolled, and expired input needs explicit operator recovery.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
LIMIT = 256 * 1024
DAY = 86400


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_regular(path, limit):
    # No FIFOs, devices or followed symlinks. Parents belong to the operator;
    # the copied public inputs, not the mutable original paths, are verified.
    flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0)
    if path.is_symlink():
        raise ValueError("linked refresh input")
    descriptor = os.open(path, flags)
    with os.fdopen(descriptor, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("refresh input is not a regular file")
        data = stream.read(limit + 1)
    if not data or len(data) > limit:
        raise ValueError("empty or oversized refresh input")
    return data


def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON field")
        result[key] = value
    return result


def time_spans(raw):
    """Use the standard decoder for every token, retaining exact source spans."""
    if not raw or len(raw) > LIMIT:
        raise ValueError("manifest size")
    text = raw.decode("utf-8")
    decoder = json.JSONDecoder(object_pairs_hook=unique)
    document = json.loads(text, object_pairs_hook=unique)
    if not isinstance(document, dict):
        raise ValueError("manifest must be an object")
    def space(at):
        while at < len(text) and text[at] in " \t\r\n":
            at += 1
        return at
    cursor = space(0)
    if text[cursor:cursor + 1] != "{":
        raise ValueError("manifest object absent")
    cursor += 1
    spans = {}
    while text[space(cursor):space(cursor) + 1] != "}":
        cursor = space(cursor)
        key, cursor = decoder.raw_decode(text, cursor)
        cursor = space(cursor)
        if not isinstance(key, str) or text[cursor:cursor + 1] != ":":
            raise ValueError("invalid top-level JSON member")
        start = space(cursor + 1)
        value, end = decoder.raw_decode(text, start)
        if key in {"released", "expires"}:
            if type(value) is not int or value < 0 or not re.fullmatch(r"[0-9]{1,20}", text[start:end]):
                raise ValueError("time must be an unsigned integer token")
            spans[key] = (start, end)
        cursor = space(end)
        if text[cursor:cursor + 1] == ",":
            cursor += 1
        elif text[cursor:cursor + 1] != "}":
            raise ValueError("invalid top-level separator")
    if set(spans) != {"released", "expires"}:
        raise ValueError("both signed times are required")
    return text, document, spans


def replace_times(raw, released, expires):
    text, _, spans = time_spans(raw)
    values = {"released": released, "expires": expires}
    for key, (start, end) in sorted(spans.items(), key=lambda item: item[1][0], reverse=True):
        text = text[:start] + str(values[key]) + text[end:]
    result = text.encode("utf-8")
    if len(result) > LIMIT:
        raise ValueError("renewed manifest size")
    return result


def preserved(raw):
    return replace_times(raw, 0, 0)


def run(signer, *arguments):
    result = subprocess.run([signer, *map(str, arguments)], check=True, capture_output=True,
                            text=True, timeout=30)
    if len(result.stdout) > 4096 or len(result.stderr) > 4096:
        raise ValueError("oversized signer response")
    keys = re.findall(r"^format=2 algorithm=ed25519\+mldsa44 key_id=([0-9a-f]{64})$", result.stdout, re.M)
    if len(keys) != 1:
        raise ValueError("signer did not return one public pair identity")
    return keys[0], result.stdout


def refresh(args):
    raw = read_regular(args.manifest, LIMIT)
    signature = read_regular(args.signature, 2528)
    public = read_regular(args.public_key, 32 * 1024)
    if not re.fullmatch(r"[0-9a-f]{64}", args.expected_manifest_sha256) or digest(raw) != args.expected_manifest_sha256:
        raise ValueError("input differs from the operator's latest trusted metadata hash")
    if not 1 <= args.valid_days <= 30:
        raise ValueError("validity must be 1..30 days")
    if not args.binary_provenance or len(args.binary_provenance.encode()) > 8192:
        raise ValueError("bounded original binary release provenance reference is required")
    if args.output.exists() or args.output.is_symlink():
        raise ValueError("choose a new output directory; existing outputs are never overwritten")
    with tempfile.TemporaryDirectory(prefix=".nuschl-refresh-", dir=args.output.parent) as temporary:
        stage = Path(temporary)
        original, original_sig, public_path = stage / "previous-update.json", stage / "previous-update.json.sig", stage / "public-trust.json"
        original.write_bytes(raw)
        original_sig.write_bytes(signature)
        public_path.write_bytes(public)
        key, status = run(args.signer, "authenticate", "--public-file", public_path,
                          "--manifest", original, "--signature-file", original_sig)
        if "authentication_only=true " not in status:
            raise ValueError("historical authentication must be explicit, never a clock override")
        _, old, _ = time_spans(raw)
        now = int(time.time())
        # Metadata issuance must move forward; the version/artifact provenance
        # does not. Do not push a client's remembered release time into the future.
        if now <= old["released"]:
            raise ValueError("clock is not after the authenticated issuance; check the clock or wait")
        if old["expires"] <= now and not args.recover_expired:
            raise ValueError("authenticated input is expired; reviewed recovery requires --recover-expired")
        expires = now + args.valid_days * DAY
        if expires <= old["expires"]:
            raise ValueError("renewal must extend validity, not shorten or roll it back")
        paired, _ = run(args.signer, "key-status", "--secret-file", args.sign_key, "--public-file", public_path)
        if paired != key:
            raise ValueError("metadata refresh cannot silently rotate the signing pair")
        renewed = replace_times(raw, now, expires)
        if preserved(renewed) != preserved(raw):
            raise ValueError("immutable artifact/source bytes changed")
        manifest = stage / "update.json"
        signed = stage / "update.json.sig"
        manifest.write_bytes(renewed)
        signed_key, _ = run(args.signer, "sign", "--secret-file", args.sign_key, "--public-file", public_path,
                            "--manifest", manifest, "--signature-file", signed)
        verified_key, _ = run(args.signer, "verify", "--public-file", public_path,
                              "--manifest", manifest, "--signature-file", signed)
        if signed_key != key or verified_key != key:
            raise ValueError("renewed signature pair differs from authenticated source")
        receipt = {"format": 1, "action": "offline-metadata-refresh-only", "key_id": key,
                   "version": old["version"], "binary_provenance_operator_reference": args.binary_provenance,
                   "binary_provenance_verified_by_helper": False,
                   "time_meaning": "released is refreshed metadata issuance, not a new binary release",
                   "previous": {"manifest_sha256": digest(raw), "signature_sha256": digest(signature),
                                "released": old["released"], "expires": old["expires"]},
                   "renewed": {"manifest_sha256": digest(renewed), "signature_sha256": digest(signed.read_bytes()),
                               "released": now, "expires": expires},
                   "raw_except_time_tokens_sha256": digest(preserved(raw)),
                   "expired_source_recovery": old["expires"] <= now,
                   "current_time_verify_passed": True, "published": False}
        (stage / "refresh-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        if args.output.exists() or args.output.is_symlink():
            raise ValueError("output appeared during signing; refusing overwrite")
        os.rename(stage, args.output)
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--signature", type=Path, required=True)
    parser.add_argument("--expected-manifest-sha256", required=True,
                        help="latest hash from the trusted operator ledger, not an untrusted download")
    parser.add_argument("--binary-provenance", required=True,
                        help="original immutable release/tag/source/build-record reference, unchanged by refresh")
    parser.add_argument("--public-key", type=Path, default=ROOT / "crates/update/update-trust.json")
    parser.add_argument("--sign-key", type=Path, required=True)
    parser.add_argument("--signer", default="nuschl-update-sign")
    parser.add_argument("--valid-days", type=int, default=30)
    parser.add_argument("--recover-expired", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    receipt = refresh(parser.parse_args())
    print(json.dumps({"version": receipt["version"], "metadata_expires": receipt["renewed"]["expires"],
                      "published": False, "review_receipt": "refresh-receipt.json"}))


if __name__ == "__main__":
    main()
