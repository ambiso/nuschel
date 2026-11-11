# Nuschl 0.1.0 — early preview

Nuschl is a Rust desktop and terminal client for Mumble, with a compatible server
and an experimental browser text client. This first release is for early testing;
interfaces and experimental protocols may still change.

- Connect to existing Mumble servers, browse their channel trees, and keep local
  chat history by default.
- Send multiline messages, images and highlighted code, and display formulas.
- Use the terminal client locally or over SSH, including a plain-text mode.
- Run `nuschld` with SQLite or PostgreSQL and the supplied NixOS module.
- Host the browser text client and its TLS tunnel relay with the web NixOS module.

## Downloads

- **Windows x86_64:** unpack the portable ZIP into a writable folder and start
  `nuschl.exe`. Keep the helper executables alongside it.
- **macOS Intel and Apple silicon:** unpack the universal ZIP and move
  `Nuschl.app` to Applications. Requires macOS 14.2 or later. Native macOS runtime
  testing remains outstanding.
- **Linux:** install through the Nix flake at this release's tag. There is no
  portable Linux binary package yet.

Windows downloads do not yet have Authenticode signatures; the macOS app is
ad-hoc signed and has not been notarised. See the [installation instructions](https://github.com/ambiso/nuschl/blob/v0.1.0/docs/install.md)
for platform requirements and first-start prompts.

## Limits

Ordinary Mumble voice and chat use transport encryption: the server can read
messages and hear voice. Experimental MLS work does not make ordinary voice
end-to-end encrypted. Video calls, screen sharing, local speech models, hosted
identity recovery, and mobile apps remain unfinished. The browser client supports
text chat and local history; browser voice, video and MLS remain unfinished.

Please report problems with the platform, release version and reproduction steps.
Avoid including private conversations, identity keys or server credentials.
