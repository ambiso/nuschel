{
  description = "Nuschl development, desktop, server, and worker builds";

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
    # Only for checking the Home Manager module; it does not use this input,
    # so consumers bring their own Home Manager.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
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
      home-manager,
      nixpkgs-mumble14,
      nixpkgs-mumble13,
      ...
    }:
    {
      lib.distroLinux = {
        mkVersion = import ./packaging/linux/version.nix;
        mkPackages = import ./packaging/linux/default.nix;
      };
      nixosModules.default = import ./nix/module.nix { inherit self; };
      # The web host: web client, share links and the tunnel relay (docs/web.md).
      nixosModules.web = import ./nix/web-module.nix { inherit self; };
      # The key-transparency auditor (docs/witness.md).
      nixosModules.witness = import ./nix/witness-module.nix { inherit self; };
      # The Mumble music bot for Subsonic servers (docs/musicbot.md).
      nixosModules.musicbot = import ./nix/musicbot-module.nix { inherit self; };
      homeModules.default = import ./nix/home-manager.nix { inherit self; };
      homeManagerModules.default = self.homeModules.default;
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
        # Only for the libFuzzer targets in fuzz/ (`nix develop .#fuzz`, `just fuzz`):
        # the newest nightly in the pinned rust-overlay that has these components.
        # llvm-tools are for `cargo fuzz coverage`. Builds and checks stay on stable.
        fuzzToolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.minimal.override {
            extensions = [
              "rust-src"
              "llvm-tools-preview"
            ];
          }
        );
        # Miri for nuschl-sandbox-windows' unsafe code (docs/windows-sandbox.md).
        miriToolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.minimal.override {
            extensions = [
              "rust-src"
              "miri"
            ];
          }
        );
        src = lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            # Trusted precompiled syntax bundle; no runtime grammar files.
            || lib.hasSuffix "/crates/content/assets/default-nix.packdump" path
            # The content-WASM integration test's fixed, tracked PNG.
            || lib.hasSuffix "/crates/content-wasm/tests/fixtures/rgba-2x2.png" path
            || lib.hasSuffix ".proto" path
            || lib.hasSuffix ".sql" path
            || lib.hasSuffix ".json" path
            || lib.hasSuffix ".vp8" path
            || lib.hasSuffix ".svg" path
            || lib.hasSuffix ".ico" path
            # The math worker's bundled font (crates/math/fonts).
            || lib.hasSuffix ".otf" path
            # Frozen identity vectors are compiled into the all-target tests.
            || lib.hasInfix "/test-vectors/" path
            # Vendored GPUI crates keep their shaders, headers and README.
            || lib.hasInfix "/vendor/gpui-pre" path
            # Vendored libraries also include their README in rustdoc. Cargo's
            # source filter does not retain mixed-case names such as Readme.md.
            || (lib.hasInfix "/vendor/" path && lib.hasSuffix ".md" path)
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
        # Twemoji images and emojibase names for both clients; the sources
        # are pinned together in nix/emoji.nix.
        emojiAssets = import ./nix/emoji.nix { inherit pkgs; };
        common = {
          pname = "nuschl";
          version = "0.1.0";
          inherit src nativeBuildInputs buildInputs;
          strictDeps = true;
          cargoExtraArgs = "--workspace";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          # Compiled into the desktop by apps/desktop/build.rs.
          NUSCHL_EMOJI_ASSETS = emojiAssets;
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
            for crate in gpui-pre gpui-pre-windows gpui-pre-apple gpui-pre-wgpu gpui-pre-linux str0m-rust-crypto ratatui-image; do
              rm -rf "$out/vendor/$crate"
              cp -R ${./vendor}/$crate "$out/vendor/$crate"
              chmod -R u+w "$out/vendor/$crate"
            done
          '';
        };
        # Artifact-only installation never executes these final-product hooks.
        # Keeping their source/notice paths in the dependency derivation would
        # rebuild dependencies after an otherwise harmless application edit.
        dependencyArgs =
          args:
          builtins.removeAttrs args [
            "src"
            "preInstall"
            "postInstall"
            "installPhase"
            "installPhaseCommand"
            # Only the desktop's own build script reads it.
            "NUSCHL_EMOJI_ASSETS"
          ];
        buildDeps = args: craneLib.buildDepsOnly (dependencyArgs args // { inherit dummySrc; });
        buildCrossPackage =
          builder: args:
          builder.buildPackage (
            args
            // {
              cargoArtifacts = builder.buildDepsOnly (
                dependencyArgs args
                // {
                  inherit dummySrc;
                  # Match Crane's implicit dependency pass; final installation still
                  # uses every hook from args. Compile/check/preBuild inputs survive.
                  installPhase = "prepareAndInstallCargoArtifactsDir";
                }
              );
            }
          );
        cargoArtifacts = buildDeps common;
        musicbotWeb = import ./nix/musicbot-web.nix { inherit pkgs lib; };
        webClient = import ./nix/web-client.nix {
          inherit
            pkgs
            lib
            toolchain
            emojiAssets
            ;
          releaseNotices = releaseNotices.web.bundle;
        };
        releaseNotices = import ./nix/release-notices.nix {
          inherit pkgs toolchain craneLib;
          projectRoot = ./.;
          webRuntimeNotices = webClient.runtimeNotices;
          musicbotRuntimeNotices = "${musicbotWeb}/licenses/runtime";
        };
        nativeNoticeScope =
          if pkgs.stdenv.hostPlatform.isDarwin then
            "macos-universal"
          else if pkgs.stdenv.hostPlatform.rust.rustcTarget == "aarch64-unknown-linux-gnu" then
            "linux-aarch64-nix"
          else
            "linux-nix";
        nativeNoticeBundle = (releaseNotices.forScope nativeNoticeScope).bundle;
        # Omit unsupported artifact names without evaluating a Linux builder
        # (or forcing pkgs for platforms that nixpkgs no longer supports).
        distroOutput = name: if system == "x86_64-linux" then name else null;
        linuxDistro = self.lib.distroLinux.mkPackages {
          inherit pkgs toolchain craneLib;
          sourceInfo = self.sourceInfo;
          nativeNotices = releaseNotices.linux.bundle;
          desktopFiles = (import ./nix/desktop-files.nix { inherit pkgs; }).linux;
          appstreamXML = ./packaging/flatpak/org.nuschl.Nuschl.metainfo.xml;
        };
        installNativeNotices = ''
          mkdir -p "$out/share/licenses/nuschl"
          cp -R ${nativeNoticeBundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
        '';
        witnessPackage = pkgs.runCommand "nuschl-witness-0.1.0" { meta.mainProgram = "nuschl-witness"; } ''
          install -Dm755 ${crates.project.workspaceMembers.nuschl-witness.build}/bin/nuschl-witness "$out/bin/nuschl-witness"
          install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
          ${installNativeNotices}
        '';
        musicbotNoticeScope = "musicbot-${pkgs.stdenv.hostPlatform.rust.rustcTarget}";
        musicbotNotices = releaseNotices.forScope musicbotNoticeScope;
        musicbotPackage =
          pkgs.runCommand "nuschl-musicbot-0.1.0"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              meta.mainProgram = "nuschl-musicbot";
            }
            ''
              install -Dm755 ${crates.project.workspaceMembers.nuschl-musicbot.build}/bin/nuschl-musicbot "$out/bin/nuschl-musicbot"
              install -Dm755 ${crates.project.workspaceMembers.nuschl-musicbot-codec-worker.build}/bin/nuschl-musicbot-codec-worker "$out/libexec/nuschl-musicbot/nuschl-musicbot-codec-worker"
              install -Dm755 ${crates.project.workspaceMembers.nuschl-content-worker.build}/bin/nuschl-content-worker "$out/libexec/nuschl-musicbot/nuschl-content-worker"
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
              mkdir -p "$out/share/licenses/nuschl"
              cp -R ${musicbotNotices.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
              wrapProgram "$out/bin/nuschl-musicbot" --set-default NUSCHL_MUSICBOT_WEB ${musicbotWeb} \
                ${lib.optionalString pkgs.stdenv.hostPlatform.isLinux "--prefix PATH : ${
                  lib.makeBinPath [
                    pkgs.bubblewrap
                    pkgs.util-linux
                  ]
                }"}
            '';
        rtcNoticeScope = "rtc-probe-${pkgs.stdenv.hostPlatform.rust.rustcTarget}";
        rtcNotices = releaseNotices.forScope rtcNoticeScope;
        rtcPackage =
          pkgs.runCommand "nuschl-rtc-probe-${common.version}"
            {
              meta.mainProgram = "nuschl-rtc-probe";
            }
            ''
              install -Dm755 ${crates.project.workspaceMembers.nuschl-rtc-probe.build}/bin/nuschl-rtc-probe "$out/bin/nuschl-rtc-probe"
              mkdir -p "$out/share/licenses/nuschl"
              cp -R ${rtcNotices.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
            '';
        serverPackage =
          pkgs.runCommand "nuschld-${common.version}"
            {
              meta.mainProgram = "nuschld";
            }
            ''
              # current_exe() locates the sibling storage worker. Symlinking the
              # server to its original crate output would lose that adjacency.
              install -Dm755 ${crates.project.workspaceMembers.nuschld.build}/bin/nuschld "$out/bin/nuschld"
              install -Dm755 ${crates.project.workspaceMembers.nuschl-storage-worker.build}/bin/nuschl-storage-worker "$out/bin/nuschl-storage-worker"
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
              ${installNativeNotices}
            '';
        # Licences, and on Linux the desktop binary under libexec (screen readers
        # announce the executable's name, not ".nuschl-wrapped") with wrappers.
        packageLayout = ''
          install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
          install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
          install -Dm644 ${./vendor/pulseaudio/LICENSE.md} "$out/share/licenses/nuschl/pulseaudio-MIT"
          install -Dm644 ${./vendor/cpal/LICENSE} "$out/share/licenses/nuschl/cpal-Apache-2.0"
          install -Dm644 ${./vendor/ratatui-image/LICENSE} "$out/share/licenses/nuschl/ratatui-image-MIT"
          install -Dm644 ${./vendor/ratatui-image/LICENSE-icy_sixel-MIT} "$out/share/licenses/nuschl/sixel-encoder-MIT"
          install -Dm644 ${./vendor/ratatui-image/NOTICE} "$out/share/licenses/nuschl/ratatui-image-NOTICE"
          install -Dm644 ${./crates/math/fonts/GUST-FONT-LICENSE.txt} "$out/share/licenses/nuschl/NewCMMath-GUST-Font-License"
          ${installNativeNotices}
        ''
        + lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
          runtimeFlags=(--prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}"
            --prefix PATH : "${
              lib.makeBinPath [
                pkgs.bubblewrap
                pkgs.util-linux
              ]
            }")
          mkdir -p "$out/libexec/nuschl"
          mv "$out/bin/nuschl" "$out/libexec/nuschl/nuschl"
          for worker in nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
            test -f "$out/bin/$worker"
            ln -s "../../bin/$worker" "$out/libexec/nuschl/$worker"
          done
          makeWrapper "$out/libexec/nuschl/nuschl" "$out/bin/nuschl" "''${runtimeFlags[@]}"
          wrapProgram "$out/bin/nuschl-cli" "''${runtimeFlags[@]}"
          # The terminal client finds the workers beside it in bin; the
          # wrapper gives their sandbox bubblewrap and the media worker its
          # audio libraries.
          wrapProgram "$out/bin/nuschl-tui" "''${runtimeFlags[@]}" --prefix PATH : "${
            lib.makeBinPath [
              pkgs.wl-clipboard-rs
              pkgs.xclip
              pkgs.wayland-utils
            ]
          }"
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
            # Workspace defaults deliberately do not enable verifier engines.
            # Reuse the two individually featured builds rather than combining
            # algorithm dependencies in a single workspace feature set.
            postInstall = ''
              install -Dm755 ${updateEdVerifier}/bin/nuschl-update-verify-ed25519 "$out/bin/nuschl-update-verify-ed25519"
              install -Dm755 ${updateMlVerifier}/bin/nuschl-update-verify-mldsa44 "$out/bin/nuschl-update-verify-mldsa44"
              ${packageLayout}
            '';
          }
        );
        # The desktop package from per-crate derivations (crate2nix): a change
        # rebuilds only the crates that depend on it.
        crates = import ./nix/crate2nix.nix { inherit pkgs toolchain; };
        # Explicit operator/CI tool; never included in client worker payloads.
        updateSigner = crates.project.workspaceMembers."nuschl-update-worker".build.override {
          features = [ "signer" ];
        };
        updateVerifier =
          feature:
          crates.project.workspaceMembers."nuschl-update-worker".build.override {
            features = [ feature ];
          };
        updateEdVerifier = updateVerifier "ed25519";
        updateMlVerifier = updateVerifier "mldsa44";
        # Explicit fixture executable, not part of the distributed payload.
        updateRuntimeFixture = updateVerifier "runtime-fixture";
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
          pkgs.runCommand "nuschl-0.1.0"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              meta.mainProgram = "nuschl";
            }
            ''
              mkdir -p "$out/bin"
              for crate in ${
                lib.concatStringsSep " " (
                  crates.binaries
                  ++ [
                    updateEdVerifier
                    updateMlVerifier
                  ]
                )
              }; do
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
        desktopCrates = "-p nuschl -p nuschl-content-worker -p nuschl-crypto-worker -p nuschl-math-worker -p nuschl-media-worker -p nuschl-storage-worker -p nuschl-transfer-worker";
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
          NUSCHL_EMOJI_ASSETS = emojiAssets;
        };
        mingw = pkgs.pkgsCross.mingwW64;
        windowsVerifiers = import ./nix/cross-updaters.nix { inherit pkgs; };
        windowsPackage = buildCrossPackage ((crane.mkLib mingw).overrideToolchain crossToolchain) (
          crossCommon
          // {
            pname = "nuschl-windows";
            __CRANE_EXPORT_CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER = "${pkgs.coreutils}/bin/false";
            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-C target-feature=+crt-static -L native=${mingw.windows.pthreads}/lib";
            postInstall = ''
              cp ${windowsVerifiers.ed25519}/bin/nuschl-update-verify-ed25519.exe "$out/bin/"
              cp ${windowsVerifiers.mldsa44}/bin/nuschl-update-verify-mldsa44.exe "$out/bin/"
              for program in nuschl nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
                test -f "$out/bin/$program.exe"
              done
              if ${pkgs.binutils}/bin/objdump -p "$out/bin/nuschl.exe" | grep -iE 'DLL Name:.*(libgcc|libstdc|libwinpthread)'; then
                echo "Windows binary depends on a MinGW runtime DLL" >&2
                exit 1
              fi
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              cp -R ${releaseNotices.windows.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
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
        macBuilder = (crane.mkLib pkgs).overrideToolchain crossToolchain;
        macBuildArgs = arch: {
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
        };
        macMinimumCheck = ''
          for program in "$out"/bin/*; do
            [ -f "$program" ] || continue
            minimum=$(${pkgs.llvmPackages.llvm}/bin/llvm-otool -l "$program" | sed -n 's/^ *minos //p' | head -1)
            if [ "$minimum" != "${macMinimum}" ]; then
              echo "$program requires macOS $minimum, expected ${macMinimum}" >&2
              exit 1
            fi
          done
        '';
        macVerifier =
          arch: feature:
          buildCrossPackage macBuilder (
            crossCommon
            // macBuildArgs arch
            // {
              pname = "nuschl-macos-${arch}-verify-${feature}";
              cargoExtraArgs = "-p nuschl-update-worker --no-default-features --features nuschl-update-worker/${feature} --bin nuschl-update-verify-${feature}";
              postInstall = ''
                test -f "$out/bin/nuschl-update-verify-${feature}"
                test "$(find "$out/bin" -maxdepth 1 -type f | wc -l)" -eq 1
              ''
              + macMinimumCheck;
            }
          );
        macSlice =
          arch:
          buildCrossPackage macBuilder (
            crossCommon
            // macBuildArgs arch
            // {
              pname = "nuschl-macos-${arch}";
              postInstall = ''
                cp ${macVerifier arch "ed25519"}/bin/nuschl-update-verify-ed25519 "$out/bin/"
                cp ${macVerifier arch "mldsa44"}/bin/nuschl-update-verify-mldsa44 "$out/bin/"
                for program in nuschl nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
                  test -f "$out/bin/$program"
                done
              ''
              + macMinimumCheck;
            }
          );
        macUniversal =
          pkgs.runCommand "nuschl-macos-universal" { nativeBuildInputs = [ pkgs.llvmPackages.llvm ]; }
            ''
              mkdir -p "$out/bin"
              for program in nuschl nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
                llvm-lipo -create ${macSlice "x86_64"}/bin/$program ${macSlice "aarch64"}/bin/$program \
                  -output "$out/bin/$program"
                llvm-lipo "$out/bin/$program" -verify_arch x86_64 arm64
              done
              if llvm-otool -L "$out/bin/nuschl" | grep -qE '^[[:space:]]+/nix/store/'; then
                echo "macOS binary depends on a Nix store library" >&2
                exit 1
              fi
            '';
        # Nuschl.app for macOS: the universal binaries, Info.plist with the
        # microphone usage text macOS requires, and an ad-hoc signature over
        # the whole bundle (rcodesign, from Linux). Updates replace every file
        # of the bundle, signature included.
        macApp =
          pkgs.runCommand "nuschl-macos-app"
            {
              nativeBuildInputs = [ pkgs.rcodesign ];
              version = crossCommon.version;
            }
            ''
              app="$out/Nuschl.app"
              mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
              cp ${macUniversal}/bin/* "$app/Contents/MacOS/"
              mkdir -p "$app/Contents/Resources/licenses"
              cp -R ${releaseNotices.macos.bundle}/share/licenses/nuschl/. "$app/Contents/Resources/licenses/"
              chmod 755 "$app"/Contents/MacOS/*
              cat > "$app/Contents/Info.plist" <<PLIST
              <?xml version="1.0" encoding="UTF-8"?>
              <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
              <plist version="1.0">
              <dict>
                <key>CFBundleExecutable</key><string>nuschl</string>
                <key>CFBundleIdentifier</key><string>org.nuschl.Nuschl</string>
                <key>CFBundleName</key><string>Nuschl</string>
                <key>CFBundleDisplayName</key><string>Nuschl</string>
                <key>CFBundleIconFile</key><string>Nuschl</string>
                <key>CFBundlePackageType</key><string>APPL</string>
                <key>CFBundleShortVersionString</key><string>$version</string>
                <key>CFBundleVersion</key><string>$version</string>
                <key>LSMinimumSystemVersion</key><string>${macMinimum}</string>
                <key>NSHighResolutionCapable</key><true/>
                <key>NSMicrophoneUsageDescription</key><string>Nuschl sends your voice to the people in your channel.</string>
                <key>NSAudioCaptureUsageDescription</key><string>Nuschl removes what your speakers play from your microphone, so others do not hear it. The sound stays on this Mac.</string>
              </dict>
              </plist>
              PLIST
              install -Dm644 ${./LICENSE} "$app/Contents/Resources/EUPL-1.2"
              install -Dm644 ${
                (import ./nix/desktop-files.nix { inherit pkgs; }).macIcon
              } "$app/Contents/Resources/Nuschl.icns"
              rcodesign sign "$app"
            '';
      in
      {
        packages.default = package;
        packages.${distroOutput "linux-native-stage"} = linuxDistro.nativeStage;
        packages.${distroOutput "deb"} = linuxDistro.deb;
        packages.${distroOutput "arch-bin"} = linuxDistro.archBin;
        packages.${distroOutput "deb-source"} = linuxDistro.debSource;
        packages.${distroOutput "deb-source-check"} = linuxDistro.debSourceCheck;
        legacyPackages.crate2nixInputs = crate2nixInputs;
        packages.default-crane = cranePackage;
        packages.emojiAssets = emojiAssets;
        packages.server = serverPackage;
        packages.witness = witnessPackage;
        packages.update-signer = updateSigner;
        packages.update-verifier-ed25519 = updateEdVerifier;
        packages.update-verifier-mldsa44 = updateMlVerifier;
        packages.update-runtime-fixture = updateRuntimeFixture;
        apps.update-signer = flake-utils.lib.mkApp {
          drv = updateSigner;
          exePath = "/bin/nuschl-update-sign";
        };
        packages.musicbot = musicbotPackage;
        packages.musicbot-web = musicbotWeb;
        packages.release-notices-musicbot = musicbotNotices.bundle;
        packages.windows = windowsPackage;
        packages.windows-update-ed25519 = windowsVerifiers.ed25519;
        packages.windows-update-mldsa44 = windowsVerifiers.mldsa44;
        packages.macos-x86_64-update-ed25519 = macVerifier "x86_64" "ed25519";
        packages.macos-x86_64-update-mldsa44 = macVerifier "x86_64" "mldsa44";
        packages.macos-aarch64-update-ed25519 = macVerifier "aarch64" "ed25519";
        packages.macos-aarch64-update-mldsa44 = macVerifier "aarch64" "mldsa44";
        packages.macos-universal = macUniversal;
        packages.macos-app = macApp;
        packages.macos-aarch64 = macSlice "aarch64";
        packages.rtc-probe = rtcPackage;
        # The web client's tunnel relay and its probe (docs/web.md), from cached crates.
        packages.web-relay = crates.project.workspaceMembers."nuschl-web-relay".build;
        packages.web-client = webClient.app;
        packages.web-wasm = webClient.wasm;
        packages.web-audio-wasm = webClient.audioWasm;
        packages.web-content-wasm = webClient.contentWasm;
        packages.web-runtime-notices = webClient.runtimeNotices;
        packages.release-license-inventory = releaseNotices.all.inventory;
        packages.release-license-compact-inventory = releaseNotices.all.compactInventory;
        packages.release-notices-linux = releaseNotices.linux.bundle;
        packages.release-notices-windows = releaseNotices.windows.bundle;
        packages.release-notices-macos = releaseNotices.macos.bundle;
        packages.release-notices-web = releaseNotices.web.bundle;
        packages.release-rtc-license-inventory = rtcNotices.inventory;
        packages.release-notices-rtc = rtcNotices.bundle;
        # Keep share-link routes at the root; the Svelte client lives at /app/.
        packages.web-site = pkgs.runCommand "nuschl-web-site" { } ''
          cp -R ${./web/site} "$out"
          chmod -R u+w "$out"
          mkdir -p "$out/app"
          cp -R ${webClient.app}/. "$out/app/"
          mkdir -p "$out/licenses/nuschl"
          cp -R ${releaseNotices.web.bundle}/share/licenses/nuschl/. "$out/licenses/nuschl/"
        '';
        apps.rtc-probe =
          (flake-utils.lib.mkApp {
            drv = rtcPackage;
            exePath = "/bin/nuschl-rtc-probe";
          })
          // {
            meta.description = "Local-only str0m audio/video transport trial";
          };
        apps.default =
          (flake-utils.lib.mkApp {
            drv = package;
            exePath = "/bin/nuschl";
          })
          // {
            meta.description = "Nuschl Mumble desktop client";
          };
        # `nix run .#tui -- mumble://name@host`: the terminal client.
        apps.tui =
          (flake-utils.lib.mkApp {
            drv = package;
            exePath = "/bin/nuschl-tui";
          })
          // {
            meta.description = "Nuschl in a terminal";
          };
        checks = {
          ${distroOutput "distro-metadata"} = linuxDistro.metadataTests;
          ${distroOutput "distro-source-assets"} = linuxDistro.assetBinding;
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
          update-ipc-policy = crates.project.workspaceMembers."nuschl-ipc".build.override {
            runTests = true;
            testInputs = [
              pkgs.bubblewrap
              pkgs.util-linux
            ];
          };
          # The launch commands each role gets; their builder needs the helpers on PATH.
          sandbox-unit = crates.project.workspaceMembers."nuschl-sandbox".build.override {
            runTests = true;
            testInputs = [
              pkgs.bubblewrap
              pkgs.util-linux
            ];
          };
          # exchange.rs has its own main (harness=false); the unit-test leaf
          # alone compiles it under libtest and reports zero tests.
          ipc-exchange = import ./nix/ipc-exchange-check.nix {
            inherit pkgs toolchain;
            ipc = crates.project.workspaceMembers."nuschl-ipc".build;
          };
          ipc-exchange-negative-controls = pkgs.linkFarm "nuschl-ipc-exchange-negative-controls" (
            lib.mapAttrsToList (name: path: {
              inherit name path;
            }) self.checks.${system}.ipc-exchange.negativeControls
          );
          update-unit = crates.project.workspaceMembers."nuschl-update".build.override { runTests = true; };
          # Scoped unit-test leaves over the cached per-crate graph (no Cargo target).
          desktop-unit = crates.project.workspaceMembers."nuschl".build.override { runTests = true; };
          core-unit = crates.project.workspaceMembers."nuschl-core".build.override { runTests = true; };
          protocol-unit = crates.project.workspaceMembers."nuschl-protocol".build.override {
            runTests = true;
          };
          client-unit = crates.project.workspaceMembers."nuschl-client".build.override {
            runTests = true;
          };
          session-unit = crates.project.workspaceMembers."nuschl-session".build.override {
            runTests = true;
          };
          content-worker-unit = crates.project.workspaceMembers."nuschl-content-worker".build.override {
            runTests = true;
            testInputs = [
              pkgs.bubblewrap
              pkgs.util-linux
            ];
          };
          content-unit = crates.project.workspaceMembers."nuschl-content".build.override {
            runTests = true;
          };
          link-preview-unit = crates.project.workspaceMembers."nuschl-link-preview".build.override {
            runTests = true;
            testInputs = [ pkgs.python3 ];
          };
          server-unit = crates.project.workspaceMembers."nuschld".build.override { runTests = true; };
          media-unit = crates.project.workspaceMembers."nuschl-media-worker".build.override {
            runTests = true;
          };
          update-crypto = crates.project.workspaceMembers."nuschl-update-worker".build.override {
            features = [ "signer" ];
            runTests = true;
          };
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
            # Share the individually reviewed exceptions with cargo-deny. Audit
            # scans every lockfile entry, including OpenMLS's unselected RustCrypto
            # test provider: only that unused graph retains crypto-bigint 0.5.
            # cargo-deny must still reject RUSTSEC-2026-0387 if it becomes reachable.
            cargoAuditExtraArgs =
              "--deny unsound --ignore RUSTSEC-2026-0387"
              +
                lib.concatMapStrings (item: " --ignore ${item.id}")
                  (builtins.fromTOML (builtins.readFile ./deny.toml)).advisories.ignore;
            # Registry-dependent yanked checks remain enabled in `cargo deny check`.
            preBuild = ''
              mkdir -p .cargo
              cp ${pkgs.writeText "nuschl-sandbox-audit.toml" "[yanked]\nenabled = false\n"} .cargo/audit.toml
            '';
          };
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          let
            homeManagerChecks = import ./nix/home-manager-test.nix {
              inherit pkgs home-manager;
              module = self.homeModules.default;
              nuschl = package;
            };
          in
          {
            preview-worker = import ./nix/preview-worker-check.nix {
              inherit pkgs toolchain;
              ipc = crates.project.workspaceMembers."nuschl-ipc".build;
              contentWorker = crates.project.workspaceMembers."nuschl-content-worker".build;
            };
            rename-options =
              let
                legacy = nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    self.nixosModules.witness
                    self.nixosModules.web
                    {
                      system.stateVersion = "26.05";
                      services.nuschel.settings.welcome_text = "legacy alias";
                      services.nuschel-witness.name = "legacy-alias";
                      services.nuschel-web.hostName = "legacy.example.invalid";
                    }
                  ];
                };
              in
              assert legacy.config.services.nuschl.settings.welcome_text == "legacy alias";
              assert legacy.config.services.nuschl-witness.name == "legacy-alias";
              assert legacy.config.services.nuschl-web.hostName == "legacy.example.invalid";
              pkgs.runCommand "nuschl-rename-options" { } "touch $out";
            home-manager = homeManagerChecks.files;
            vm-home-manager = homeManagerChecks.vm;
            # nuschld under the module with stock clients (`nix build .#checks.x86_64-linux.vm-server`).
            vm-server = import ./nix/vm-server.nix {
              inherit pkgs serverPackage;
              nuschl = package;
              module = self.nixosModules.default;
              witnessModule = self.nixosModules.witness;
            };
            # The web host's module: site, landing page, app links, and a login
            # to Murmur through the relay (`nix build .#checks.x86_64-linux.vm-web`).
            vm-web = import ./nix/vm-web.nix {
              inherit pkgs;
              relay = crates.project.workspaceMembers."nuschl-web-relay".build;
              module = self.nixosModules.web;
            };
            # The music bot under its module, against Murmur and a fake library
            # (`nix build .#checks.x86_64-linux.vm-musicbot`).
            vm-musicbot = import ./nix/vm-musicbot.nix {
              inherit pkgs;
              musicbot = musicbotPackage;
              cli = crates.project.workspaceMembers.nuschl-cli.build;
              module = self.nixosModules.musicbot;
            };
            vm-smoke = import ./nix/vm-smoke.nix {
              inherit pkgs;
              nuschl = package;
              rtcProbe = rtcPackage;
              module = self.nixosModules.default;
              legacyServers = {
                "1.4" = "${nixpkgs-mumble14.legacyPackages.${system}.murmur}/bin/mumble-server";
                "1.3" = "${nixpkgs-mumble13.legacyPackages.${system}.murmur}/bin/murmurd";
              };
            };
            vm-link-previews = import ./tests/link_previews_vm.nix {
              inherit pkgs;
              nuschl = package;
              sourceIdentity = self.rev or self.outPath;
              contentWorker = crates.project.workspaceMembers."nuschl-content-worker".build;
              # Export the actual cached test ELF without running its private
              # network fixture in a build sandbox or rewriting its bytes.
              nativeFetchTest =
                (crates.project.workspaceMembers."nuschl-session".build.override {
                  runTests = true;
                  testPreRun = ''
                    case "$f" in
                      */nuschl_session-*)
                        test ! -e "$out/bin/nuschl-preview-fetch-test"
                        install -Dm0555 "$f" "$out/bin/nuschl-preview-fetch-test"
                        ;;
                    esac
                    continue
                  '';
                }).test.overrideAttrs
                  { dontFixup = true; };
            };
          }
        );
        devShells.default = craneLib.devShell {
          inherit buildInputs;
          LIBCLANG_PATH = common.LIBCLANG_PATH;
          NUSCHL_EMOJI_ASSETS = emojiAssets;
          packages =
            nativeBuildInputs
            ++ [
              pkgs.cargo-deny
              pkgs.cargo-audit
              pkgs.just
              pkgs.gh
              # Release notes; release/local-release.py runs it offline with cliff.toml.
              pkgs.git-cliff
              pkgs.nixfmt
              pkgs.taplo
              pkgs.jujutsu
              pkgs.python3
              pkgs.nodejs
            ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.bubblewrap
              pkgs.util-linux
            ];
          LD_LIBRARY_PATH = lib.optionalString pkgs.stdenv.hostPlatform.isLinux (
            lib.makeLibraryPath buildInputs
          );
        };
        # cargo-fuzz with nightly; libfuzzer-sys compiles libFuzzer with the
        # shell's C++ compiler. See SECURITY.md, "Fuzzing".
        devShells.fuzz = pkgs.mkShell {
          packages = [
            fuzzToolchain
            pkgs.cargo-fuzz
            pkgs.just
            pkgs.protobuf
            pkgs.pkg-config
          ];
        };
        # `just miri`: the sandbox adapter's tests under Miri, for Linux and Windows.
        devShells.miri = pkgs.mkShell {
          packages = [
            miriToolchain
            pkgs.just
          ];
        };
        formatter = pkgs.nixfmt;
      }
    );
}
