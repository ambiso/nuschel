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
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      advisory-db,
      ...
    }:
    {
      nixosModules.default = import ./nix/module.nix { inherit self; };
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        inherit (pkgs) lib;
        # flake.lock pins rust-overlay, including what it considers latest stable.
        toolchain = pkgs.rust-bin.stable.latest.default.override {
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
            || lib.hasSuffix ".sql" path
            || lib.hasSuffix ".json" path;
        };
        nativeBuildInputs = [
          pkgs.pkg-config
          pkgs.protobuf
          pkgs.cmake
          pkgs.clang
          pkgs.makeWrapper
        ];
        buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
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
          pname = "nuschel";
          version = "0.1.0";
          inherit src nativeBuildInputs buildInputs;
          strictDeps = true;
          cargoExtraArgs = "--workspace";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };
        cargoArtifacts = craneLib.buildDepsOnly common;
        serverCommon = common // {
          pname = "nuscheld";
          cargoExtraArgs = "-p nuscheld -p nuschel-storage-worker";
          buildInputs = [ ];
        };
        serverPackage = craneLib.buildPackage (
          serverCommon
          // {
            cargoArtifacts = craneLib.buildDepsOnly serverCommon;
            postInstall = ''
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschel/EUPL-1.2"
              install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschel/Mumble-BSD"
            '';
          }
        );
        package = craneLib.buildPackage (
          common
          // {
            inherit cargoArtifacts;
            postInstall = ''
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschel/EUPL-1.2"
              install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschel/Mumble-BSD"
            ''
            + lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              wrapProgram "$out/bin/nuschel" \
                --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}" \
                --prefix PATH : "${
                  lib.makeBinPath [
                    pkgs.bubblewrap
                    pkgs.util-linux
                  ]
                }"
            '';
          }
        );
      in
      {
        packages.default = package;
        packages.server = serverPackage;
        apps.default =
          (flake-utils.lib.mkApp {
            drv = package;
            exePath = "/bin/nuschel";
          })
          // {
            meta.description = "Nuschel GPUI desktop client foundation";
          };
        checks = {
          build = package;
          server-build = serverPackage;
          clippy = craneLib.cargoClippy (
            common
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          fmt = craneLib.cargoFmt { inherit src; };
          deny = craneLib.cargoDeny (
            common
            // {
              cargoExtraArgs = "";
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
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm-smoke = import ./nix/vm-smoke.nix {
            inherit pkgs;
            nuschel = package;
            module = self.nixosModules.default;
          };
        };
        devShells.default = craneLib.devShell {
          inherit buildInputs;
          LIBCLANG_PATH = common.LIBCLANG_PATH;
          packages =
            nativeBuildInputs
            ++ [
              pkgs.cargo-deny
              pkgs.cargo-audit
              pkgs.just
              pkgs.nixfmt
              pkgs.taplo
              pkgs.jujutsu
              pkgs.python3
            ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.bubblewrap
              pkgs.util-linux
            ];
          LD_LIBRARY_PATH = lib.optionalString pkgs.stdenv.hostPlatform.isLinux (
            lib.makeLibraryPath buildInputs
          );
        };
        formatter = pkgs.nixfmt;
      }
    );
}
