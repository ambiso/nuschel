# SPDX-License-Identifier: EUPL-1.2
# What the "ci" check builds for pull requests and pushes to main:
# `nix build -f ci.nix`. Every flake check, the NixOS VM tests included,
# except the release packaging proofs, which releases build.
let
  checks = (builtins.getFlake ("git+file://" + toString ./.)).checks.${builtins.currentSystem};
in
builtins.removeAttrs checks [
  "distro-metadata"
  "distro-source-assets"
]
