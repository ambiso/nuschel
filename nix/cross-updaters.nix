# SPDX-License-Identifier: EUPL-1.2
# Production Windows verification engines have independent Cargo feature roots.
# No fixture-only import-library workaround or signing/runtime-fixture feature.
{ pkgs }:
let
  cross = pkgs.pkgsCross.mingwW64;
  # Keep this target-only compiler closure identical to the proven narrow
  # Windows route; adding Darwin stdlib components must not evict this cache.
  toolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "x86_64-pc-windows-gnu" ];
  };
  graph = import ./crate2nix/Cargo.nix {
    pkgs = cross;
    defaultCrateOverrides = cross.defaultCrateOverrides // {
      nuschl-update-worker = _: {
        extraRustcOpts = [
          "-C"
          "target-feature=+crt-static"
          "-L"
          "native=${cross.windows.pthreads}/lib"
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
  # The pinned buildRustCrate final move assumes native binaries lack a suffix.
  # Scope the .exe correction to these final crates, failing on upstream drift.
  originalLib = pkgs.path + "/pkgs/build-support/rust/build-rust-crate/lib.sh";
  originalText = builtins.readFile originalLib;
  before = ''mv "$out_dir/$crate_name_" "$out_dir/$crate_name"'';
  after = ''mv "$out_dir/$crate_name_.exe" "$out_dir/$crate_name.exe"'';
  windowsLib =
    assert pkgs.lib.hasInfix before originalText;
    pkgs.writeText "nuschl-cross-updater-lib.sh" (
      builtins.replaceStrings [ before ] [ after ] originalText
    );
  build =
    feature:
    (graph.workspaceMembers.nuschl-update-worker.build.override {
      features = [ feature ];
    }).overrideAttrs
      (old: {
        configurePhase =
          assert builtins.replaceStrings [ "${originalLib}" ] [ "" ] old.configurePhase != old.configurePhase;
          builtins.replaceStrings [ "${originalLib}" ] [ "${windowsLib}" ] old.configurePhase;
        postInstall = (old.postInstall or "") + ''
          test -f "$out/bin/nuschl-update-verify-${feature}.exe"
          test "$(find "$out/bin" -maxdepth 1 -type f | wc -l)" -eq 1
          if ${pkgs.binutils}/bin/objdump -p "$out/bin/nuschl-update-verify-${feature}.exe" | grep -iE 'DLL Name:.*(libgcc|libstdc|libwinpthread)'; then
            echo "Windows verifier depends on a MinGW runtime DLL" >&2
            exit 1
          fi
        '';
      });
in
{
  ed25519 = build "ed25519";
  mldsa44 = build "mldsa44";
}
