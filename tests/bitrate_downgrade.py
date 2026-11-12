#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Profiles with a chosen voice bitrate, opened by an earlier Nuschl.

The new terminal client chooses 256 kb/s in a profile that also has
relay-only on (settings.json) and a managed setting (managed.json, as Home
Manager writes it, with the managed bitrate in its own sidecar). The released
0.1.0 client must then open the profile without setting any file aside, and
the new one must still find 256 kb/s. Controls: the same values written the
naive way (in settings.json, in managed.json) are refused by 0.1.0, which is
the hazard this avoids. No server, audio or network is used.

    python3 tests/bitrate_downgrade.py <new binaries> <0.1.0 binaries> --output <directory>
"""
import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from tui_loopback import Terminal, environment  # noqa: E402


def run(binaries, profile, root, lines, wait):
    """The plain terminal client on `profile`; what it printed."""
    tui = Terminal("tui", [str((binaries / "nuschl-tui").resolve()), "--plain", "--no-audio",
                           "--opener", "/bin/true", "--profile-dir", str(profile)], environment(root), plain=True)
    tui.wait_for("Nuschl in plain lines")
    for line, expect in zip(lines, wait):
        tui.line(line)
        tui.wait_for(expect)
    tui.close("/quit\r")
    return tui.output


def files(profile):
    config = profile / "config"
    return sorted(path.name for path in config.iterdir() if not path.name.startswith("."))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("new", type=Path)
    parser.add_argument("old", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    report = {}

    # 1. The new client chooses 256 kb/s; settings.json and managed.json are
    #    what Home Manager and the user wrote.
    profile, root = out / "profile", out / "root"
    (profile / "config").mkdir(parents=True)
    settings = {"relay_only": True, "enter_sends": False}
    (profile / "config/settings.json").write_text(json.dumps(settings))
    (profile / "config/managed.json").write_text(json.dumps({"settings": {"send_typing": False}}))
    chosen = run(args.new, profile, root, ["/bitrate 256"], ["you asked for 256 kb/s"])
    assert json.loads((profile / "config/voice-quality.json").read_text()) == {"version": 1, "voice_bitrate": 256000}
    assert json.loads((profile / "config/settings.json").read_text()) == settings
    report["new_wrote"] = files(profile)

    # 2. The released version opens it: nothing set aside, no problem shown.
    old = run(args.old, profile, root, ["/status"], ["No server."])
    assert "could not be read" not in old and "invalid" not in old.lower(), old
    assert files(profile) == report["new_wrote"], files(profile)
    assert json.loads((profile / "config/settings.json").read_text()) == settings
    report["old_output"] = old

    # 3. Back on the new version: the bitrate and relay-only are both kept.
    again = run(args.new, profile, root, ["/bitrate"], ["you asked for 256 kb/s"])
    assert "could not be read" not in again, again
    report["new_again"] = "you asked for 256 kb/s" in again

    # 4. Managed bitrate as Home Manager writes it: managed.json keeps old keys.
    (profile / "config/managed-voice-quality.json").write_text(json.dumps({"version": 1, "voice_bitrate": 96000}))
    managed_new = run(args.new, profile, root, ["/bitrate"], ["you asked for 96 kb/s"])
    assert "set by your system configuration" in managed_new, managed_new
    managed_old = run(args.old, profile, root, ["/status"], ["No server."])
    assert "system configuration is invalid" not in managed_old, managed_old
    assert files(profile) == sorted(report["new_wrote"] + ["managed-voice-quality.json"])

    # Controls: the naive places are refused by the released version.
    naive = out / "naive"
    (naive / "config").mkdir(parents=True)
    (naive / "config/settings.json").write_text(json.dumps({**settings, "voice_bitrate": 256000}))
    (naive / "config/managed.json").write_text(json.dumps({"settings": {"send_typing": False, "voice_bitrate": 96000}}))
    control = run(args.old, naive, out / "naive-root", ["/status"], ["No server."])
    assert "renamed to settings.json.invalid" in control, control
    assert "system configuration is invalid" in control, control
    assert (naive / "config/settings.json.invalid").exists()
    report["control_old_output"] = control
    report["control_files"] = files(naive)
    (out / "bitrate-downgrade.json").write_text(json.dumps(report, indent=2))
    print("Released 0.1.0 reads profiles with a chosen bitrate; the naive placement is refused: ok")


if __name__ == "__main__":
    main()
