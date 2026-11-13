# SPDX-License-Identifier: EUPL-1.2
# What the "ci" check builds for pull requests and pushes to main:
# `nix build -f ci.nix`. That is every flake check except the NixOS VM
# fixtures (vm-*) and the release packaging proofs (distro-*).
let
  checks = (builtins.getFlake (toString ./.)).checks.${builtins.currentSystem};
  slow = name: builtins.match "(vm|distro)-.*" name != null;
in
builtins.removeAttrs checks (builtins.filter slow (builtins.attrNames checks))
