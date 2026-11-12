# SPDX-License-Identifier: EUPL-1.2
# Review wiring for nix/ipc-exchange-check.nix before flake.nix includes it.
# Rebuilds pkgs, the toolchain and the crate2nix graph as flake.nix does, and
# refuses unless its nuschl-ipc library is the flake's own derivation (the
# one the update-ipc-policy check links), so the check reuses cached leaves.
#   nix build --impure -f nix/ipc-exchange-check-wiring.nix check
#   nix build --impure -f nix/ipc-exchange-check-wiring.nix negativeControls
#   nix build --impure -f nix/ipc-exchange-check-wiring.nix replaced-by-true  # must fail
{
  flake ? builtins.getFlake (toString ../.),
  system ? builtins.currentSystem,
}:
let
  pkgs = import flake.inputs.nixpkgs {
    inherit system;
    overlays = [ (import flake.inputs.rust-overlay) ];
  };
  inherit (pkgs) lib;
  toolchain = pkgs.rust-bin.stable.latest.default.override {
    extensions = [ "rust-src" ];
    targets = [ "wasm32-unknown-unknown" ];
  };
  crates = import ./crate2nix.nix { inherit pkgs toolchain; };
  ipc = crates.project.workspaceMembers."nuschl-ipc".build;
  # The flake's crate2nix leaves are not exposed; its update-ipc-policy check
  # links the nuschl-ipc library, so that derivation names it as an input.
  policy = builtins.readFile flake.checks.${system}.update-ipc-policy.drvPath;
  check = import ./ipc-exchange-check.nix { inherit pkgs toolchain ipc; };
in
assert lib.assertMsg (lib.hasInfix (builtins.unsafeDiscardStringContext ipc.drvPath) policy)
  "this wiring's nuschl-ipc ${ipc.drvPath} is not the flake's update-ipc-policy input";
{
  inherit check;
  inherit (check) diagnostic negativeControls;
  replaced-by-true = check.runWith "${pkgs.coreutils}/bin/true";
}
