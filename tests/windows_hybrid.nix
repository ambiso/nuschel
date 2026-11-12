# SPDX-License-Identifier: EUPL-1.2
# Test-only cross products, no production trust input or desktop compilation.
# nix build --impure --file tests/windows_hybrid.nix ed ml fixture --cores 2 --max-jobs 2
# The feature roots remain separate; only the non-distributed fixture enables
# signing to generate its own temporary pair inside the guest.
let
  root = ../.;
  flake = builtins.getFlake "git+file://${toString root}";
  pkgs = import flake.inputs.nixpkgs {
    system = "x86_64-linux";
    overlays = [ (import flake.inputs.rust-overlay) ];
  };
  cross = pkgs.pkgsCross.mingwW64;
  toolchain = pkgs.rust-bin.stable.latest.default.override { targets = [ "x86_64-pc-windows-gnu" ]; };
  # buildRustCrate does not retain the old windows-targets import archive from
  # its build script's source directory. Expose only its exact locked .a file.
  windowsImport =
    pkgs.runCommand "windows-0.52.6-gnu-import-library"
      {
        src = pkgs.fetchurl {
          url = "https://static.crates.io/crates/windows_x86_64_gnu/windows_x86_64_gnu-0.52.6.crate";
          sha256 = "0y0sifqcb56a56mvn7xjgs8g43p33mfqkd8wj1yhrgxzma05qyhl";
        };
      }
      ''
        mkdir -p "$out"
        tar -xf "$src" -C "$out" --strip-components=1 windows_x86_64_gnu-0.52.6/lib
      '';
  graph = import (root + "/nix/crate2nix/Cargo.nix") {
    pkgs = cross;
    defaultCrateOverrides = cross.defaultCrateOverrides // {
      nuschl-update-worker = _: {
        extraRustcOpts = [
          "-C"
          "target-feature=+crt-static"
          "-L"
          "native=${cross.windows.pthreads}/lib"
          "-L"
          "native=${windowsImport}/lib"
        ];
      };
    };
    buildRustCrateForPkgs =
      cratePkgs:
      cratePkgs.buildRustCrate.override {
        rustc = toolchain;
        cargo = toolchain;
      };
  };
  # Pinned buildRustCrate lib.sh handles wasm suffixes but assumes native bins
  # have no extension. This local test builder changes that final move only;
  # production cross packages and repository/global Nixpkgs remain untouched.
  originalLib = pkgs.path + "/pkgs/build-support/rust/build-rust-crate/lib.sh";
  windowsLib = pkgs.writeText "lib.sh" (
    builtins.replaceStrings
      [ ''mv "$out_dir/$crate_name_" "$out_dir/$crate_name"'' ]
      [ ''mv "$out_dir/$crate_name_.exe" "$out_dir/$crate_name.exe"'' ]
      (builtins.readFile originalLib)
  );
  build =
    feature:
    (graph.workspaceMembers.nuschl-update-worker.build.override { features = [ feature ]; })
    .overrideAttrs
      (old: {
        configurePhase =
          builtins.replaceStrings [ "${originalLib}" ] [ "${windowsLib}" ]
            old.configurePhase;
      });
in
{
  ed = build "ed25519";
  ml = build "mldsa44";
  fixture = build "runtime-fixture";
}
