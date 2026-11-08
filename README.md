# Nuschel

Nuschel is an EUPL-1.2 Rust workspace for a Mumble-compatible desktop client and
server. GPUI is the desktop renderer. Shared state, protocol framing, and message
presentation do not depend on GPUI, leaving mobile and web adapters open.

This is a foundation, not a working voice client or server yet. It includes bounded
protocol/worker framing, content processing, local history storage, audio policy,
and independent worker executables. Session negotiation, network transports, native
audio routing, permissions, and protocol interoperability are subsequent milestones.

## Development

```sh
direnv allow
# Or enter the same pinned environment explicitly:
nix develop
just build
just check
```

`cargo add` is built into the pinned Cargo. `Cargo.lock` and `flake.lock` are tracked.
`nix flake check` runs the native build, tests, Clippy, formatting, dependency policy,
and a vulnerability check using a pinned advisory database. `cargo deny check` also
checks the current advisory database and registry yanks outside the Nix sandbox.

Use `jj` for incremental commits. See [the architecture](docs/architecture.md),
[the safety policy](SECURITY.md), and [the integration test plan](tests/README.md).

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
