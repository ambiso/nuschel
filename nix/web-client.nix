# Reproducible browser SDK and UI. No ignored developer output is an input.
{
  pkgs,
  lib,
  toolchain,
}:
let
  native = import ./crate2nix.nix { inherit pkgs toolchain; };
  # Nix calls freestanding targets "none"; Rust's browser target is "unknown".
  # rust.platform follows rustcTarget, so crate2nix sees the correct browser cfg.
  wasmPkgs = import pkgs.path {
    localSystem = pkgs.stdenv.buildPlatform.system;
    crossSystem = {
      config = "wasm32-unknown-none";
      libc = "none";
      rust.rustcTarget = "wasm32-unknown-unknown";
    };
  };
  overrides = pkgs.defaultCrateOverrides // {
    nuschl-protocol = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };
    ring = _: {
      nativeBuildInputs = [
        pkgs.llvmPackages.clang-unwrapped
        pkgs.llvmPackages.bintools-unwrapped
      ];
      CC_wasm32_unknown_unknown = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
      AR_wasm32_unknown_unknown = "${pkgs.llvmPackages.bintools-unwrapped}/bin/llvm-ar";
    };
  };
  project = import ./crate2nix/Cargo.nix { pkgs = wasmPkgs; };
  # crate2nix 0.15 resolves transitive build-dependency cfgs against the final
  # target. Keep prost-build's filesystem dependencies on the native graph.
  built = project.internal.builtRustCratesWithFeatures {
    packageId = "nuschl-client-wasm";
    features = [ "default" ];
    runTests = false;
    buildRustCrateForPkgsFunc =
      cratePkgs: args:
      let
        isWasm = cratePkgs.stdenv.hostPlatform.isWasm;
        crate =
          (cratePkgs.buildRustCrate.override {
            rustc = toolchain;
            cargo = toolchain;
            defaultCrateOverrides = overrides;
            stdenv = if isWasm then cratePkgs.stdenvNoCC else cratePkgs.stdenv;
          })
            (
              args
              // lib.optionalAttrs (args.crateName == "nuschl-protocol") {
                buildDependencies = native.project.workspaceMembers.nuschl-protocol.build.buildDependencies;
              }
            );
      in
      crate.overrideAttrs (
        old:
        lib.optionalAttrs isWasm {
          # buildRustCrate currently hard-codes Unix and omits target features.
          # Match rustc's actual target cfg, particularly wasm-bindgen externrefs.
          configurePhase =
            builtins.replaceStrings
              [ ''export CARGO_CFG_TARGET_FAMILY="unix"'' "export CARGO_CFG_UNIX=1" ]
              [ ''export CARGO_CFG_TARGET_FAMILY="wasm"'' "unset CARGO_CFG_UNIX" ]
              old.configurePhase;
          preConfigure = (old.preConfigure or "") + ''
            export CARGO_CFG_TARGET_FEATURE="$(${toolchain}/bin/rustc --target wasm32-unknown-unknown --print cfg | sed -n 's/^target_feature="\(.*\)"$/\1/p' | paste -sd, -)"
          '';
        }
      );

  };
  wasm =
    pkgs.runCommand "nuschl-web-sdk"
      {
        nativeBuildInputs = [ pkgs.wasm-bindgen-cli_0_2_130 ];
      }
      ''
        mkdir -p "$out"
        wasm-bindgen --target web --out-name nuschl_client_wasm --out-dir "$out" ${lib.getLib built.crates.nuschl-client-wasm}/lib/nuschl_client_wasm-*.wasm
      '';
  app = pkgs.buildNpmPackage {
    pname = "nuschl-web-app";
    version = "0.1.0";
    src = lib.cleanSourceWith {
      src = ../web/app;
      filter =
        path: type:
        !(builtins.elem (builtins.baseNameOf path) [
          "node_modules"
          "dist"
          "sdk"
        ]);
    };
    npmDepsHash = "sha256-zDYMAXXhJzcB09V/CnuclOqngsob7YP2cMFiGL1J5SE=";
    postPatch = ''
      mkdir -p src/sdk
      cp ${wasm}/* src/sdk/
    '';
    npmBuildFlags = [
      "--"
      "--base=/app/"
    ];
    doCheck = true;
    checkPhase = "node --test src/*.test.js";
    installPhase = ''
      mkdir -p "$out"
      cp -R dist/. "$out/"
    '';
  };
in
{
  inherit app wasm;
}
