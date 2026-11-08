{
  description = "Nuschel development, desktop, server, and worker builds";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    {
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      advisory-db,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        inherit (pkgs) lib;
        toolchain = pkgs.rust-bin.stable."1.95.0".default.override {
          extensions = [ "rust-src" ];
          targets = [ "wasm32-unknown-unknown" ];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        src = lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || lib.hasSuffix ".proto" path
            || lib.hasSuffix ".json" path;
        };
        nativeBuildInputs = [
          pkgs.pkg-config
          pkgs.protobuf
          pkgs.cmake
          pkgs.clang
        ];
        buildInputs = lib.optionals pkgs.stdenv.isLinux [
          pkgs.alsa-lib
          pkgs.pipewire
          pkgs.libpulseaudio
          pkgs.fontconfig
          pkgs.freetype
          pkgs.libxkbcommon
          pkgs.wayland
          pkgs.vulkan-loader
          pkgs.libGL
          pkgs.libx11
          pkgs.libxcursor
          pkgs.libxi
          pkgs.libxrandr
          pkgs.libxcb
        ];
        common = {
          inherit src nativeBuildInputs buildInputs;
          strictDeps = true;
          cargoExtraArgs = "--workspace";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };
        cargoArtifacts = craneLib.buildDepsOnly common;
        package = craneLib.buildPackage (common // { inherit cargoArtifacts; });
      in
      {
        packages.default = package;
        apps.default = flake-utils.lib.mkApp {
          drv = package;
          exePath = "/bin/nuschel";
        };
        checks = {
          build = package;
          clippy = craneLib.cargoClippy (
            common
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
            }
          );
          fmt = craneLib.cargoFmt { inherit src; };
          deny = craneLib.cargoDeny (
            common
            // {
              cargoDenyChecks = "bans licenses sources";
            }
          );
          audit = craneLib.cargoAudit {
            inherit src advisory-db;
            cargoAuditExtraArgs = "--deny unsound";
            # Registry-dependent yanked checks remain enabled in `cargo deny check`.
            preBuild = ''
              mkdir -p .cargo
              cp ${pkgs.writeText "nuschel-sandbox-audit.toml" "[yanked]\nenabled = false\n"} .cargo/audit.toml
            '';
          };
        };
        devShells.default = craneLib.devShell {
          inherit nativeBuildInputs buildInputs;
          LIBCLANG_PATH = common.LIBCLANG_PATH;
          packages = [
            pkgs.cargo-deny
            pkgs.cargo-audit
            pkgs.just
            pkgs.nixfmt
            pkgs.taplo
            pkgs.jujutsu
          ]
          ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.bubblewrap ];
          LD_LIBRARY_PATH = lib.optionalString pkgs.stdenv.isLinux (lib.makeLibraryPath buildInputs);
        };
        formatter = pkgs.nixfmt;
      }
    );
}
