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
    # Older stock servers for interoperability tests only (legacy UDP format).
    nixpkgs-mumble14.url = "github:NixOS/nixpkgs/nixos-22.11";
    nixpkgs-mumble13.url = "github:NixOS/nixpkgs/nixos-21.05";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      advisory-db,
      nixpkgs-mumble14,
      nixpkgs-mumble13,
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
            || lib.hasSuffix ".json" path
            || lib.hasSuffix ".vp8" path
            || lib.hasSuffix ".svg" path
            # Vendored GPUI platform crates keep their shaders and headers.
            || lib.hasInfix "/vendor/gpui-pre-" path
            || lib.hasPrefix "LICENSE" (builtins.baseNameOf path);
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
        # Registry CPAL uses this patched path dependency. Its real API must
        # survive Crane's dummy-source pass so CPAL can compile against it.
        pulseSource = lib.cleanSourceWith {
          src = ./vendor/pulseaudio;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || lib.hasPrefix "LICENSE" (builtins.baseNameOf path)
            || lib.hasSuffix ".md" path;
        };
        dummySrc = craneLib.mkDummySrc {
          inherit src;
          extraDummyScript = ''
            rm -rf "$out/vendor/pulseaudio"
            cp -R ${pulseSource} "$out/vendor/pulseaudio"
            chmod -R u+w "$out/vendor/pulseaudio"
            # GPUI compiles against these patched crates during the dependency pass.
            for crate in gpui-pre-windows gpui-pre-apple; do
              rm -rf "$out/vendor/$crate"
              cp -R ${./vendor}/$crate "$out/vendor/$crate"
              chmod -R u+w "$out/vendor/$crate"
            done
          '';
        };
        buildDeps =
          args: craneLib.buildDepsOnly ((builtins.removeAttrs args [ "src" ]) // { inherit dummySrc; });
        cargoArtifacts = buildDeps common;
        serverCommon = common // {
          pname = "nuscheld";
          cargoExtraArgs = "-p nuscheld -p nuschel-storage-worker";
          buildInputs = [ ];
        };
        rtcCommon = common // {
          pname = "nuschel-rtc-probe";
          cargoExtraArgs = "-p nuschel-rtc -p nuschel-rtc-probe";
          buildInputs = [ ];
        };
        rtcPackage = craneLib.buildPackage (
          rtcCommon
          // {
            cargoArtifacts = buildDeps rtcCommon;
            postInstall = ''
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschel/EUPL-1.2"
            '';
          }
        );
        serverPackage = craneLib.buildPackage (
          serverCommon
          // {
            cargoArtifacts = buildDeps serverCommon;
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
              install -Dm644 ${./vendor/pulseaudio/LICENSE.md} "$out/share/licenses/nuschel/pulseaudio-MIT"
            ''
            + lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              for program in nuschel nuschel-cli; do
                wrapProgram "$out/bin/$program" \
                --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}" \
                --prefix PATH : "${
                  lib.makeBinPath [
                    pkgs.bubblewrap
                    pkgs.util-linux
                  ]
                }"
              done
            '';
          }
        );
        # Cross builds from Linux. A separate toolchain keeps native checks cached.
        crossToolchain =
          p:
          p.pkgsBuildHost.rust-bin.stable.latest.default.override {
            targets = [
              "x86_64-pc-windows-gnu"
              "x86_64-apple-darwin"
              "aarch64-apple-darwin"
            ];
          };
        desktopCrates = "-p nuschel -p nuschel-content-worker -p nuschel-media-worker -p nuschel-storage-worker";
        crossCommon = {
          version = "0.1.0";
          inherit src dummySrc;
          strictDeps = true;
          doCheck = false; # Tests run natively, never under an emulator.
          cargoExtraArgs = desktopCrates;
          nativeBuildInputs = [
            pkgs.protobuf
            pkgs.pkg-config
          ];
        };
        mingw = pkgs.pkgsCross.mingwW64;
        windowsPackage = ((crane.mkLib mingw).overrideToolchain crossToolchain).buildPackage (
          crossCommon
          // {
            pname = "nuschel-windows";
            __CRANE_EXPORT_CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER = "${pkgs.coreutils}/bin/false";
            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-C target-feature=+crt-static -L native=${mingw.windows.pthreads}/lib";
            postInstall = ''
              for program in nuschel nuschel-content-worker nuschel-media-worker nuschel-storage-worker; do
                test -f "$out/bin/$program.exe"
              done
              if ${pkgs.binutils}/bin/objdump -p "$out/bin/nuschel.exe" | grep -iE 'DLL Name:.*(libgcc|libstdc|libwinpthread)'; then
                echo "Windows binary depends on a MinGW runtime DLL" >&2
                exit 1
              fi
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschel/EUPL-1.2"
            '';
          }
        );
        # Pinned SDK, as in ../opendeck-akp05. 14.x because cpal's CoreAudio
        # backend links the process-tap API (macOS 14.2+), which Rust cannot
        # weak-link; the deployment target follows.
        macSdk = pkgs.fetchzip {
          url = "https://github.com/joseluisq/macosx-sdks/releases/download/14.5/MacOSX14.5.sdk.tar.xz";
          hash = "sha256-YABLNOdte3WE8mnUm32zSlQ3lyDFQNnxXTW9gj51MX0=";
        };
        macSlice =
          arch:
          ((crane.mkLib pkgs).overrideToolchain crossToolchain).buildPackage (
            crossCommon
            // {
              pname = "nuschel-macos-${arch}";
              nativeBuildInputs = crossCommon.nativeBuildInputs ++ [
                pkgs.cargo-zigbuild
                pkgs.zig
              ];
              CARGO_BUILD_TARGET = "${arch}-apple-darwin";
              cargoBuildCommand = "cargoWithProfile zigbuild --target ${arch}-apple-darwin";
              cargoCheckCommand = "true";
              SDKROOT = macSdk;
              MACOSX_DEPLOYMENT_TARGET = "14.2";
              preBuild = ''
                export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
                export CARGO_ZIGBUILD_CACHE_DIR="$TMPDIR/zigbuild-cache"
              '';
            }
          );
        macUniversal =
          pkgs.runCommand "nuschel-macos-universal" { nativeBuildInputs = [ pkgs.llvmPackages.llvm ]; } ''
            mkdir -p "$out/bin"
            for program in nuschel nuschel-content-worker nuschel-media-worker nuschel-storage-worker; do
              llvm-lipo -create ${macSlice "x86_64"}/bin/$program ${macSlice "aarch64"}/bin/$program \
                -output "$out/bin/$program"
              llvm-lipo "$out/bin/$program" -verify_arch x86_64 arm64
            done
            if llvm-otool -L "$out/bin/nuschel" | grep -qE '^[[:space:]]+/nix/store/'; then
              echo "macOS binary depends on a Nix store library" >&2
              exit 1
            fi
          '';
      in
      {
        packages.default = package;
        packages.server = serverPackage;
        packages.windows = windowsPackage;
        packages.macos-universal = macUniversal;
        packages.macos-aarch64 = macSlice "aarch64";
        packages.rtc-probe = rtcPackage;
        apps.rtc-probe =
          (flake-utils.lib.mkApp {
            drv = rtcPackage;
            exePath = "/bin/nuschel-rtc-probe";
          })
          // {
            meta.description = "Local-only str0m audio/video transport trial";
          };
        apps.default =
          (flake-utils.lib.mkApp {
            drv = package;
            exePath = "/bin/nuschel";
          })
          // {
            meta.description = "Nuschel Mumble desktop client";
          };
        checks = {
          build = package;
          server-build = serverPackage;
          rtc-build = rtcPackage;
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
            # RUSTSEC-2023-0071: rsa only generates the local Mumble key (see deny.toml).
            cargoAuditExtraArgs = "--deny unsound --ignore RUSTSEC-2023-0071";
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
            legacyServers = {
              "1.4" = "${nixpkgs-mumble14.legacyPackages.${system}.murmur}/bin/mumble-server";
              "1.3" = "${nixpkgs-mumble13.legacyPackages.${system}.murmur}/bin/murmurd";
            };
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
