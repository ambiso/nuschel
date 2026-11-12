#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual packaged welcome documents through an owned loopback Murmur and relay."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import shutil
import subprocess
from local_murmur import Murmur, eventually, free_port
from web_audio import prepare_browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("site", "relay", "murmur", "wrapper", "browser-tmp", "playwright"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--prepare-browser", action="store_true")
    parser.add_argument("--chromium", type=Path)
    parser.add_argument("--font-dir", type=Path)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    if args.prepare_browser:
        if not args.chromium or not args.font_dir:
            parser.error("--prepare-browser requires --chromium and --font-dir")
        report = prepare_browser(args.output / "browser", args.chromium, shutil.which("bwrap"), args.font_dir)
        # A fresh fontconfig intentionally has no host aliases. Give both UI
        # text and mathematics their intended cached local system-font family.
        aliases = {"sans-serif": "DejaVu Sans", "ui-sans-serif": "DejaVu Sans",
                   "system-ui": "DejaVu Sans", "Inter": "DejaVu Sans",
                   "serif": "DejaVu Serif", "monospace": "DejaVu Sans Mono",
                   "math": "DejaVu Math TeX Gyre"}
        config = args.output / "browser/fonts.conf"
        text = config.read_text().replace("</fontconfig>", "".join(
            f"<alias><family>{generic}</family><prefer><family>{family}</family></prefer></alias>"
            for generic, family in aliases.items()) + "</fontconfig>")
        config.write_text(text)
        report["font_aliases"] = aliases
        report["fontconfig_sha256"] = hashlib.sha256(config.read_bytes()).hexdigest()
        (args.output / "browser/confinement.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))
        return
    if not all((args.site, args.relay, args.murmur, args.wrapper, args.browser_tmp, args.playwright)):
        parser.error("--site, --relay, --murmur, --wrapper, --browser-tmp and --playwright are required")
    root = args.output / "server"
    root.mkdir(exist_ok=True)
    murmur = Murmur(args.murmur, root, free_port())
    source = ("Domain emphasis: <b>something.de</b><br/>"
              "Linked emphasis: <a href='https://linked.test/'><b>linked.test</b></a><br/>"
              "Literal: &lt;em&gt;literal.test&lt;/em&gt;<br/><br/>"
              "```rust<br/>let host = &quot;code.test&quot;;<br/>```<br/>"
              "<img src='/canary-welcome'><img src='https://tracker.invalid/canary-remote'>"
              "<script>hiddenWelcome()</script>"
              + "<br/>".join(f"Welcome line {i}: " + "éλ" * 20 for i in range(35)))
    lines = murmur.ini.read_text().splitlines()
    murmur.ini.write_text("\n".join('welcometext="' + source + '"' if line.startswith("welcometext=") else line for line in lines) + "\n")
    murmur.seed()
    murmur.start()
    port = free_port()
    config = args.output / "relay.toml"
    config.write_text(f'listen="127.0.0.1:{port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
    with (args.output / "relay.log").open("w") as log:
        relay = subprocess.Popen([str(args.relay), "--config", str(config)], stdout=log, stderr=log)
        try:
            def listening():
                with socket.socket() as probe:
                    return probe.connect_ex(("127.0.0.1", port)) == 0
            eventually(listening, "private relay failed to start")
            subprocess.run([
                "node", str(Path(__file__).with_suffix(".browser.mjs")), str(args.site),
                f"ws://127.0.0.1:{port}/relay", str(murmur.port), str(args.wrapper),
                str(args.playwright), str(args.output),
            ], env={**os.environ, "TMPDIR": str(args.browser_tmp)}, timeout=120, check=True)
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            murmur.stop()


if __name__ == "__main__":
    main()
