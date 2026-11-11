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
            # The release signing key the updater trusts.
            || lib.hasSuffix "update-key.pub" path
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
        # Licences, and on Linux the desktop binary under libexec (screen readers
        # announce the executable's name, not ".nuschel-wrapped") with wrappers.
        packageLayout = ''
          install -Dm644 ${./LICENSE} "$out/share/licenses/nuschel/EUPL-1.2"
          install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschel/Mumble-BSD"
          install -Dm644 ${./vendor/pulseaudio/LICENSE.md} "$out/share/licenses/nuschel/pulseaudio-MIT"
        ''
        + lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
          runtimeFlags=(--prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}"
            --prefix PATH : "${
              lib.makeBinPath [
                pkgs.bubblewrap
                pkgs.util-linux
              ]
            }")
          mkdir -p "$out/libexec/nuschel"
          mv "$out/bin/nuschel" "$out/libexec/nuschel/nuschel"
          for worker in nuschel-content-worker nuschel-media-worker nuschel-storage-worker; do
            ln -s "../../bin/$worker" "$out/libexec/nuschel/$worker"
          done
          makeWrapper "$out/libexec/nuschel/nuschel" "$out/bin/nuschel" "''${runtimeFlags[@]}"
          wrapProgram "$out/bin/nuschel-cli" "''${runtimeFlags[@]}"
          # Desktop entry and hicolor icons; shells match the window by its app id.
          cp -R --no-preserve=mode ${
            (import ./nix/desktop-files.nix { inherit pkgs; }).linux
          }/share/. "$out/share/"
        '';
        # Whole-workspace build (crane): kept for comparison and as a fallback.
        cranePackage = craneLib.buildPackage (
          common
          // {
            inherit cargoArtifacts;
            postInstall = packageLayout;
          }
        );
        # The desktop package from per-crate derivations (crate2nix): a change
        # rebuilds only the crates that depend on it.
        crates = import ./nix/crate2nix.nix { inherit pkgs toolchain; };
        # What nix/crate2nix/Cargo.nix was generated from: Cargo.lock and every
        # manifest. `just crate2nix` records it; a check compares.
        crate2nixInputs =
          let
            manifests =
              dir:
              map (name: ./. + "/${dir}/${name}/Cargo.toml") (
                builtins.attrNames (
                  lib.filterAttrs (
                    name: type: type == "directory" && builtins.pathExists (./. + "/${dir}/${name}/Cargo.toml")
                  ) (builtins.readDir (./. + "/${dir}"))
                )
              );
          in
          builtins.hashString "sha256" (
            lib.concatMapStrings builtins.readFile (
              [
                ./Cargo.lock
                ./Cargo.toml
              ]
              ++ manifests "apps"
              ++ manifests "crates"
              ++ manifests "vendor"
            )
          );
        package =
          pkgs.runCommand "nuschel-0.1.0"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              meta.mainProgram = "nuschel";
            }
            ''
              mkdir -p "$out/bin"
              for crate in ${lib.concatStringsSep " " crates.binaries}; do
                for program in "$crate"/bin/*; do
                  cp "$program" "$out/bin/"
                done
              done
              chmod -R u+w "$out/bin"
              ${packageLayout}
            '';
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
        macMinimum = "14.2";
        # cargo-zigbuild gives zig an unversioned macOS target, for which zig
        # assumes its newest release (15.0) whatever MACOSX_DEPLOYMENT_TARGET says.
        cargoZigbuild = pkgs.cargo-zigbuild.overrideAttrs (old: {
          patches = (old.patches or [ ]) ++ [ ./nix/cargo-zigbuild-macos-version.patch ];
        });
        macSlice =
          arch:
          ((crane.mkLib pkgs).overrideToolchain crossToolchain).buildPackage (
            crossCommon
            // {
              pname = "nuschel-macos-${arch}";
              nativeBuildInputs = crossCommon.nativeBuildInputs ++ [
                cargoZigbuild
                pkgs.zig
              ];
              CARGO_BUILD_TARGET = "${arch}-apple-darwin";
              cargoBuildCommand = "cargoWithProfile zigbuild --target ${arch}-apple-darwin";
              cargoCheckCommand = "true";
              SDKROOT = macSdk;
              MACOSX_DEPLOYMENT_TARGET = macMinimum;
              preBuild = ''
                export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
                export CARGO_ZIGBUILD_CACHE_DIR="$TMPDIR/zigbuild-cache"
              '';
              postInstall = ''
                for program in "$out"/bin/*; do
                  [ -f "$program" ] || continue
                  minimum=$(${pkgs.llvmPackages.llvm}/bin/llvm-otool -l "$program" | sed -n 's/^ *minos //p' | head -1)
                  if [ "$minimum" != "${macMinimum}" ]; then
                    echo "$program requires macOS $minimum, expected ${macMinimum}" >&2
                    exit 1
                  fi
                done
              '';
            }
          );
        macUniversal =
          pkgs.runCommand "nuschel-macos-universal" { nativeBuildInputs = [ pkgs.llvmPackages.llvm ]; }
            ''
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
        # Nuschel.app for macOS: the universal binaries, Info.plist with the
        # microphone usage text macOS requires, and an ad-hoc signature over
        # the whole bundle (rcodesign, from Linux). Updates replace every file
        # of the bundle, signature included.
        macApp =
          pkgs.runCommand "nuschel-macos-app"
            {
              nativeBuildInputs = [ pkgs.rcodesign ];
              version = crossCommon.version;
            }
            ''
              app="$out/Nuschel.app"
              mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
              cp ${macUniversal}/bin/* "$app/Contents/MacOS/"
              chmod 755 "$app"/Contents/MacOS/*
              cat > "$app/Contents/Info.plist" <<PLIST
              <?xml version="1.0" encoding="UTF-8"?>
              <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
              <plist version="1.0">
              <dict>
                <key>CFBundleExecutable</key><string>nuschel</string>
                <key>CFBundleIdentifier</key><string>org.nuschel.Nuschel</string>
                <key>CFBundleName</key><string>Nuschel</string>
                <key>CFBundleDisplayName</key><string>Nuschel</string>
                <key>CFBundleIconFile</key><string>Nuschel</string>
                <key>CFBundlePackageType</key><string>APPL</string>
                <key>CFBundleShortVersionString</key><string>$version</string>
                <key>CFBundleVersion</key><string>$version</string>
                <key>LSMinimumSystemVersion</key><string>${macMinimum}</string>
                <key>NSHighResolutionCapable</key><true/>
                <key>NSMicrophoneUsageDescription</key><string>Nuschel sends your voice to the people in your channel.</string>
                <key>NSAudioCaptureUsageDescription</key><string>Nuschel removes what your speakers play from your microphone, so others do not hear it. The sound stays on this Mac.</string>
              </dict>
              </plist>
              PLIST
              install -Dm644 ${./LICENSE} "$app/Contents/Resources/EUPL-1.2"
              install -Dm644 ${
                (import ./nix/desktop-files.nix { inherit pkgs; }).macIcon
              } "$app/Contents/Resources/Nuschel.icns"
              rcodesign sign "$app"
            '';
      in
      {
        packages.default = package;
        legacyPackages.crate2nixInputs = crate2nixInputs;
        packages.default-crane = cranePackage;
        packages.server = serverPackage;
        packages.windows = windowsPackage;
        packages.macos-universal = macUniversal;
        packages.macos-app = macApp;
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
          # nix/crate2nix/Cargo.nix must follow Cargo.lock and the manifests.
          crate2nix-current =
            let
              recorded = lib.removeSuffix "\n" (builtins.readFile ./nix/crate2nix/inputs.sha256);
            in
            pkgs.runCommand "crate2nix-current" { } (
              if recorded == crate2nixInputs then
                "touch $out"
              else
                ''
                  echo "nix/crate2nix/Cargo.nix is out of date: run 'just crate2nix'" >&2
                  exit 1
                ''
            );
          audit = craneLib.cargoAudit {
            inherit src advisory-db;
            # RUSTSEC-2023-0071: rsa only generates the local Mumble key; RUSTSEC-2026-0459:
            # SQLite opens only Nuschel's own database (see deny.toml).
            cargoAuditExtraArgs = "--deny unsound --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2026-0459";
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
