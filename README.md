# Nuschel

Nuschel is an EUPL-1.2 Rust workspace for a Mumble-compatible desktop client and
server. GPUI is the desktop renderer. Shared state, protocol framing, and message
presentation do not depend on GPUI, leaving mobile and web adapters open.

This is a foundation, not a working voice client or server yet. It includes bounded
protocol/worker framing, content processing, local history storage, audio policy,
and independent worker executables. Session negotiation, network transports, native
audio routing, permissions, and protocol interoperability are subsequent milestones.

The GPUI shell includes development messages demonstrating preserved newlines,
expandable long messages, and syntax highlighting through the content worker.

## Development

```sh
direnv allow
# Or enter the same pinned environment explicitly:
nix develop
just build
just check
just workers
# Linux VM packaging, confinement, SQLx/PostgreSQL and service preflight:
just vm
```

`cargo add` is built into the pinned Cargo. `Cargo.lock` and `flake.lock` are tracked.
`nix flake check` runs the native build, tests, Clippy, formatting, dependency policy,
and a vulnerability check using a pinned advisory database. `cargo deny check` also
checks the current advisory database and registry yanks outside the Nix sandbox.

Use `jj` for incremental commits. See [the architecture](docs/architecture.md),
[the safety policy](SECURITY.md), and [the integration test plan](tests/README.md).

On NixOS, the desktop loads the host's Vulkan driver. The flake's glibc must be new
enough to load that driver; an older nixpkgs pin can produce `No GPU adapters found`
even when other applications work. Diagnose driver loading with
`VK_LOADER_DEBUG=error,warn RUST_LOG=nuschel=info,gpui_wgpu=info nix run .`.
If the log reports a missing `GLIBC_*` version, update the pin with
`nix flake update nixpkgs` and rebuild. Adding driver search paths cannot fix that
ABI mismatch.

## Server module and databases

The flake exports `nixosModules.default` and a headless `packages.<system>.server`
containing `nuscheld` and the storage worker, without the GPUI/audio stack. The module
defaults to SQLite and also supports PostgreSQL, either through `database.urlFile`
or local peer authentication:

```nix
imports = [ inputs.nuschel.nixosModules.default ];
services.nuschel = {
  enable = true;
  database.type = "postgresql";
  database.createLocally = true;
};
```

The module is a development interface: its database preflight is implemented, but
`nuscheld` deliberately refuses to start a listener until Mumble sessions exist.
Enabling it is not yet a deployable voice server. The VM smoke test substitutes a
configuration-only command while retaining the real hardened unit and migrations.

External PostgreSQL URLs and TLS keys come from runtime credential files, not inline
Nix values. `openFirewall` is off by default. The service uses DynamicUser, a private
state directory, restricted permissions and systemd hardening. See
[module options](nix/module.nix) for the configuration interface.

## Message defaults

- Newlines and blank lines remain visible. Ordinary text is literal; HTML is inert.
- Long messages collapse with a keyboard-accessible **Show more** control.
- Fenced code is highlighted in the content worker with Syntect's Rust regex backend.
- Local chat history is enabled by default, including on legacy Mumble servers.
  Legacy history contains messages observed locally, not messages sent while offline.

These rules live in shared crates; a mobile renderer can implement the same behavior.

## License

Nuschel is licensed under [EUPL-1.2](LICENSE). Vendored Mumble schemas retain their
[upstream BSD license](crates/protocol/proto/LICENSE.mumble). Dependency license
allowances are reviewed separately in `deny.toml`; SPDX allowlisting does not replace
release attribution or license-compliance review.
