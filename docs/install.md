# Installing and updating Nuschl

The README has the short version. This page has the details: the Nix package and
the Home Manager module, the Windows and macOS downloads, how releases are made and
signed, and how Nuschl updates itself. No release has been published yet; the
first publication still requires enrolled signing trust and complete notices ([How releases are made](#how-releases-are-made)).

## Nix and NixOS

The flake builds the desktop app for Linux (`packages.<system>.default`, tested on
x86_64) and exports a NixOS module for the server and a Home Manager module for the
app. `nix run github:ambiso/nuschl` starts the app without installing it, and
`nix run github:ambiso/nuschl#tui` the terminal client (`nuschl-tui`, in the same
package). To install it system-wide from your own flake:

```nix
# inputs.nuschl.url = "github:ambiso/nuschl";
environment.systemPackages = [ inputs.nuschl.packages.${pkgs.stdenv.hostPlatform.system}.default ];
```

The package installs the desktop entry and icons and registers `mumble://` links. It
starts every worker inside bubblewrap, which it brings along. Nix installations
update through Nix (`nix flake update nuschl`), never by themselves.

The package (`nix build .#default`, also used by the VM tests) is built per crate with
crate2nix; [development.md](development.md) explains the build.

**`No GPU adapters found` on NixOS.** The desktop loads the host's Vulkan driver, and
the flake's glibc must be new enough to load it; an older nixpkgs pin can produce
this error even when other applications work. Diagnose driver loading with
`VK_LOADER_DEBUG=error,warn RUST_LOG=nuschl=info,gpui_wgpu=info nix run .`. If the
log reports a missing `GLIBC_*` version, update the pin with
`nix flake update nixpkgs` and rebuild. Adding driver search paths cannot fix that
ABI mismatch.

## Home Manager

The flake exports a Home Manager module, `homeModules.default` (also
`homeManagerModules.default`). It installs the package with its desktop entry and
icons and works with your own nixpkgs: only the package comes from this flake, and
`programs.nuschl.package` replaces it.

```nix
# Flake input: nuschl.url = "github:ambiso/nuschl";
imports = [ inputs.nuschl.homeModules.default ];
programs.nuschl = {
  enable = true;
  # Start minimised at login (Linux), with the entry Settings → General writes.
  autostart = true;
  # Applied on every start; read-only in the app.
  settings = {
    enter_sends = false;
    shortcuts."push-to-talk" = "f13";
    audio.transmit = "push_to_talk";
    # Privacy → Relay-only connections; no direct peer sockets or IP discovery.
    relay_only = true;
  };
  servers = [
    { name = "Team"; host = "voice.example.org"; username = "alice"; }
  ];
  # Written only where Nuschl has no settings.json or servers.json yet.
  initialSettings.sound_volume = 0.5;
  initialServers = [
    { name = "Friends"; host = "mumble.example.net"; username = "alice"; }
  ];
};
```

`settings` and `servers` go into a read-only `managed.json` in Nuschl's folder
(`~/.config/nuschl`, or `~/Library/Application Support/org.Nuschl.Nuschl`).
Nuschl never writes it. On every start its settings are merged key by key over
`settings.json` and win; Settings shows those controls dimmed with "Set by your
system configuration", and `settings.json` keeps only your own values, which
return once a key is no longer managed. Its bookmarks cannot be removed and only
take a password in their form; Nuschl keeps what it learns about them (an
accepted certificate in `servers.json`, the channel tree in the cache, the last
channel in the state folder), and a bookmark removed from `servers` disappears on
the next start. A `nuschl/managed.json` in `$XDG_CONFIG_DIRS` (such as `/etc/xdg`)
sets system-wide defaults under the user's own (profiles opened with
`--profile-dir` skip it).

`initialSettings` and `initialServers` only seed a new profile: once the files
exist, Nuschl owns them and later changes to these options do not apply. A bookmark
in `servers` with the host, port and username of one in `initialServers` takes it
over. With `autostart`, turning the switch off in the app lasts until the next
`home-manager switch`.

An invalid `managed.json` is reported and ignored until it is fixed. The check
`.#checks.x86_64-linux.vm-home-manager` boots a VM in which logging in starts
Nuschl and managed settings show as set by the system configuration and stay so.

## Other Linux distributions

There is no package outside Nix yet (no Flatpak, AppImage or distribution package).
Building from source works with Cargo ([development.md](development.md)). At run time
Nuschl needs a Vulkan driver, and `bwrap` (bubblewrap) and `prlimit` (util-linux) on
the `PATH`: it refuses to start a worker it cannot sandbox.

## Windows

Each release publishes a portable zip, `nuschl-<version>-windows-x86_64.zip`.
Unpack it into a folder you can write to (your user folder, not Program Files) and
start `nuschl.exe`. There is no installer, Start menu entry or uninstaller yet: Nuschl
updates itself in the folder it was unpacked to, and deleting the folder removes it.
Settings and history live in `%APPDATA%\Nuschl` and `%LOCALAPPDATA%\Nuschl`
([desktop.md](desktop.md#where-nuschl-keeps-its-files)).

Releases are **not code-signed** (a decision for now: no Authenticode certificate).
The first start of a downloaded copy therefore shows Microsoft Defender SmartScreen's
"Windows protected your PC". Choose **More info**, check that the app is `nuschl.exe`
from an unknown publisher, and choose **Run anyway**. Unblocking the zip before
unpacking it (**Properties → Unblock**) avoids the prompt, and updates that Nuschl
fetches itself never show it. With Smart App Control on, Windows blocks unsigned apps
and offers no way to run them anyway.

The Windows build is cross-compiled on Linux (`nix build .#windows`) and runs in an
unattended Windows 11 VM in the tests (`tests/windows_vm.py`): the GUI, connecting,
autostart, audio devices, the microphone and echo tests, in-place updates, and the
workers' AppContainer sandboxes ([windows-sandbox.md](windows-sandbox.md)). It has not
been tested on real hardware.

## macOS

Each release publishes `nuschl-<version>-macos-universal.zip` with `Nuschl.app`
for Intel and Apple silicon, for macOS 14.2 or later (the audio library links Core
Audio's process-tap API). The macOS build has **not yet been run on a Mac**.

Move `Nuschl.app` to Applications before the first start. Started straight from
Downloads, it would run from a hidden read-only copy (App Translocation), where
Nuschl cannot update itself.

The app is ad-hoc signed, not signed with a Developer ID or notarised (also a
decision for now). macOS therefore refuses the first start of a downloaded copy,
saying that it cannot verify the app is free of malware. Close that message, open
**System Settings → Privacy & Security**, scroll to Security, where it says that
Nuschl was blocked, choose **Open Anyway** and confirm with your password; macOS
then starts it and remembers the choice. Removing the quarantine flag in Terminal
does the same: `xattr -dr com.apple.quarantine /Applications/Nuschl.app`. Updates
that Nuschl fetches itself are not quarantined and start normally.

On first use macOS also asks for the microphone, and for system audio recording
when echo cancellation records what the speakers play ([desktop.md](desktop.md#voice)).

## Updates

Windows and macOS copies check for a newer release 20 seconds after start and every
six hours (**Settings → General → Check for updates**), and show a banner when one
exists. They verify the manifest's signature, accept only newer versions, refuse a
manifest released before the newest they have seen, require unexpired v2 metadata, download each file into a staging folder inside the installation and
check its hash, and swap the files by renaming, rolling back on failure; old files are
removed on the next start. The code is in `crates/update`.

## How releases are made

Tagging `vX.Y.Z` (matching the workspace version) runs
`.github/workflows/release.yml`, which calls three independent read-only platform
build jobs before a separate publication job. The flake cross-compiles the
Windows build (MinGW) and a universal `Nuschl.app` (zig and a pinned SDK, ad-hoc
signed with `rcodesign`), so no Windows or macOS runner is needed. The workflow
zips both, publishes every file of each installation as its own release asset, and
writes `update.json`, which lists every file with its size and SHA-256.

Two different kinds of signing are involved:

- **The update manifest requires Ed25519 AND ML-DSA-44**. The repository secret
  `UPDATE_HYBRID_SIGNING_KEY` holds the private pair; only public pairs compiled
  from `crates/update/update-trust.json` can authorize runtime updates. Two
  separately confined fixed-algorithm workers must both accept `update.json.sig`
  over the exact raw `update.json`. There is no minisign/adjacent-key fallback.
  The public store is deliberately unenrolled and no secret has been provisioned
  by this implementation, so publication remains blocked. Missing/incomplete
  runtime notices also block signing. See the [operator and verification
  instructions](../apps/update-worker/README.md). Metadata expires within 30 days;
  trusted periodic refresh is necessary even if binaries do not change. No
  automatic refresh/publication job has been added.
- **The programs are not code-signed** for the operating systems: no Authenticode
  certificate for Windows and no Developer ID or notarisation for macOS, so
  SmartScreen and Gatekeeper treat downloads as described above. Developer ID
  signing and notarisation could later be added to the same Linux job with
  `rcodesign` and an Apple Developer account.

What the workers' confinement does and does not provide on each platform:
[SECURITY.md](../SECURITY.md).
