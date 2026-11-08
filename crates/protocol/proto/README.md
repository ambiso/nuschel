# Vendored Mumble schemas

These schemas were copied from the source fetched with `nix build nixpkgs#mumble.src`:
`/nix/store/acw8kf5sz8rl06c87cd126ipki7hjjm7-source`.
They retain their upstream copyright and BSD license, reproduced in `LICENSE.mumble`.

| File | SHA256 |
| --- | --- |
| Mumble.proto | `0f86d85938ff2268e3eb05ce0120805fb049ad0d062f4d01c6657b048dcc9245` |
| MumbleUDP.proto | `8087983b0d9a12e11380cad99870a0ef3cee7550b13a114a733aa835acd3d040` |

`prost-build` generates Rust during the build. Nix supplies the native `protoc`.
These schemas do not implement session negotiation, legacy UDP framing, OCB encryption,
or the channel/ACL state machine; interoperability needs explicit integration tests.
