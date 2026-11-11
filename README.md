# Nuschel

![Nuschel in a voice channel: the channel tree with the people in it, and a conversation with code, a photo and a formula while Ada talks](docs/images/nuschel.png)

Nuschel is a voice and text chat app for [Mumble](https://www.mumble.info/) servers.
It works with the Mumble servers you already use, keeps your chat history, and shows
code, images and formulas in messages. Nuschel also has its own server, `nuscheld`,
which stock Mumble clients can use too: with Nuschel on both ends, the server keeps
the history, you can edit messages, and direct messages reach people who are
offline. End-to-end encryption is being built ([how far it is](#security-and-privacy)).
Nuschel's own code is memory-safe Rust, and what other people send you is handled in
sandboxed helper processes.

Nuschel is in early development: version 0.1, no published release yet, and things
still change. More about the app: [docs/desktop.md](docs/desktop.md).

## Install

### NixOS and Nix

Try it without installing anything:

```sh
nix run github:ambiso/nuschel
```

To keep it, add the flake as an input and install
`inputs.nuschel.packages.${pkgs.stdenv.hostPlatform.system}.default`, or use the Home
Manager module:

```nix
# flake.nix: inputs.nuschel.url = "github:ambiso/nuschel";
imports = [ inputs.nuschel.homeModules.default ];
programs.nuschel = {
  enable = true;
  autostart = true; # start minimised when you log in (Linux)
};
```

The module can also set settings and servers for you, which then show as read-only
in the app. The package registers `mumble://` links and updates with your flake
inputs, not by itself. Details, and what to do about `No GPU adapters found`:
[docs/install.md](docs/install.md#nix-and-nixos).

### Other Linux distributions

Not yet: there is no Flatpak, AppImage or distribution package. You can build it
from source (see [Development](#development)); it needs a Vulkan driver, and
bubblewrap (`bwrap`) for its sandbox.

### Windows

[Releases](https://github.com/ambiso/nuschel/releases) will have a portable zip,
`nuschel-<version>-windows-x86_64.zip`. Unpack it into a folder of your own (not
Program Files) and start `nuschel.exe`. There is no installer yet; Nuschel updates
itself in that folder.

Releases are not code-signed for now, so on the first start Windows shows "Windows
protected your PC". Choose **More info**, then **Run anyway**. Windows with Smart App
Control turned on refuses unsigned apps altogether. The Windows build is tested in a
Windows 11 VM.

### macOS

[Releases](https://github.com/ambiso/nuschel/releases) will have
`nuschel-<version>-macos-universal.zip` with `Nuschel.app` (Intel and Apple silicon,
macOS 14.2 or later). Move it to Applications first, so that it can update itself.
The app is not notarised for now, so macOS refuses the first start: then open
**System Settings → Privacy & Security** and choose **Open Anyway** next to the note
that Nuschel was blocked. The macOS build has not been tried on a Mac yet.

### Android, iOS, the web, a terminal client

Planned ([docs/clients.md](docs/clients.md)).

## Getting started

**Add a server.** Click **+** in the left rail and enter the server's address and the
user name you want. On its first start Nuschel creates a certificate that Mumble
servers know you by; **Settings → Identity** can export it, for stock Mumble or a
backup. If a server's certificate is self-signed, Nuschel shows its fingerprint and
asks before connecting, once.

**Links.** A `mumble://` link (`mumble://voice.example.org/Games`) opens the server and
joins that channel. Right-click a channel and choose **Copy link** to share one. With
the Linux package, links open in Nuschel from your browser too; on Windows and macOS,
click them inside Nuschel for now. A link can also carry the server's certificate
fingerprint (`?fp=sha256:…`), and then nobody needs to confirm it.

**Talking.** Click a channel to read its conversation; double-click it to join its
voice. You join muted: unmute with the microphone button at the bottom left. Nuschel
then sends your voice whenever you speak. For push to talk, open Settings (the
sliders next to the microphone), choose **Push to talk** under Transmission, and
record a key under Shortcuts. The key also works while Nuschel is in the background,
where the desktop allows system-wide shortcuts (GNOME asks you once).

**Your microphone.** **Settings → Microphone test** lets you hear yourself as others
would and check for echo, without being on a server. Nuschel cancels echo and noise
itself, or uses the system's echo cancellation on Linux when it finds one.

## Running a server

`nuscheld` is Nuschel's server. Stock Mumble clients (1.3 to 1.5) use it like the
standard Mumble server, and it can import an existing one with its users, channels
and permissions. On NixOS:

```nix
imports = [ inputs.nuschel.nixosModules.default ];
services.nuschel = {
  enable = true;
  openFirewall = true;
  security = "compatible";
};
```

It keeps its data in SQLite, or in PostgreSQL (`database.type = "postgresql"` with
`database.createLocally = true`).

There are two security modes. **Compatible**, the only one today, admits stock Mumble
clients: messages and voice are encrypted on the way, as with any Mumble server, but
the server can read them. **End-to-end** mode will admit only Nuschel clients and keep
the server out of conversations; it is not finished, and the server refuses to start
with it.

Configuration, administration, importing a Mumble server, and running it without
Nix: [docs/server.md](docs/server.md).

## Security and privacy

- **Sandboxes.** Message text and images, formulas, voice, file transfers, your local
  history and your keys are each handled by a sandboxed helper process of its own.
  Only the file-transfer helper may use the network, and only the history and key
  helpers reach a folder, each just their own. The sandboxes use bubblewrap on Linux and AppContainers on
  Windows (both tested), and Seatbelt on macOS (not yet tested). Details and
  exceptions: [SECURITY.md](SECURITY.md).
- **Encryption today.** Connections are encrypted (TLS, and Mumble's encrypted voice),
  but by default nothing is end-to-end encrypted: the server can read messages and
  hear voice. A red open lock in the app says so.
- **End-to-end encryption** is being built on MLS with post-quantum keys
  ([docs/e2ee.md](docs/e2ee.md)). Working now on Nuschel servers: an identity of your
  own that is the same on every server, and key transparency, which lets Nuschel
  notice when a contact's key changes or a server lies about it; safety numbers let
  you verify someone in person. End-to-end encrypted direct messages are an
  experiment, off by default; channels and voice come later.
- **Your data.** History is kept on your device, and on Nuschel servers also by the
  server (forever, unless the server or channel sets a shorter time). Server
  passwords are never saved. Where everything is stored:
  [docs/desktop.md](docs/desktop.md#where-nuschel-keeps-its-files).

## Development

Everything comes from the flake's development shell:

```sh
direnv allow          # or: nix develop
just build            # build everything
just desktop          # build and start the app
just check            # formatting, Clippy, tests, licences and advisories
just vm               # Linux integration tests in NixOS VMs
just fuzz <target>    # fuzz a parser; `just fuzz-list` lists them
just readme-screenshot  # regenerate the picture above
```

Windows and macOS builds are cross-compiled on Linux (`nix build .#windows`,
`.#macos-app`). More in [docs/development.md](docs/development.md), what the tests
cover in [tests/README.md](tests/README.md), how it fits together in
[docs/architecture.md](docs/architecture.md), and what is next in [TODO.md](TODO.md).

## Licence

Nuschel is licensed under the [EUPL-1.2](LICENSE). Vendored Mumble schemas keep their
[upstream BSD licence](crates/protocol/proto/LICENSE.mumble). Dependency licences are
reviewed in `deny.toml`; that allowlist does not replace attribution or a
licence-compliance review for releases.
