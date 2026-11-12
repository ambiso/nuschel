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
  buildWasm =
    packageId:
    project.internal.builtRustCratesWithFeatures {
      inherit packageId;
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
                // lib.optionalAttrs (isWasm && args.crateName == "nuschl-content-wasm") {
                  # The content worker has no native process rlimit. Bound its
                  # actual linear memory, in addition to decoder/job limits.
                  extraRustcOpts = (args.extraRustcOpts or [ ]) ++ [
                    "-C"
                    "link-arg=--max-memory=268435456"
                  ];
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
  bindings =
    {
      packageId,
      moduleName,
      name,
    }:
    pkgs.runCommand name
      {
        nativeBuildInputs = [ pkgs.wasm-bindgen-cli_0_2_130 ];
      }
      ''
        mkdir -p "$out"
        wasm-bindgen --target web --out-name ${moduleName} --out-dir "$out" ${
          lib.getLib (buildWasm packageId).crates.${packageId}
        }/lib/${moduleName}-*.wasm
      '';
  wasm = bindings {
    packageId = "nuschl-client-wasm";
    moduleName = "nuschl_client_wasm";
    name = "nuschl-web-sdk";
  };
  audioWasm = bindings {
    packageId = "nuschl-audio-wasm";
    moduleName = "nuschl_audio_wasm";
    name = "nuschl-web-audio-codec";
  };
  contentWasm = bindings {
    packageId = "nuschl-content-wasm";
    moduleName = "nuschl_content_wasm";
    name = "nuschl-web-content-codec";
  };
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
          "audio-codec"
          "content-codec"
        ]);
    };
    npmDepsHash = "sha256-zDYMAXXhJzcB09V/CnuclOqngsob7YP2cMFiGL1J5SE=";
    postPatch = ''
      mkdir -p src/sdk src/audio-codec src/content-codec
      cp ${wasm}/* src/sdk/
      cp ${audioWasm}/* src/audio-codec/
      cp ${contentWasm}/* src/content-codec/
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
  inherit
    app
    wasm
    audioWasm
    contentWasm
    ;
}
