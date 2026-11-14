# Per-crate Nix builds (crate2nix): every crate is its own derivation, so a
# change rebuilds only the crates that depend on it, and unchanged crates come
# from the store. `nix/crate2nix/Cargo.nix` is generated from Cargo.lock; run
# `just crate2nix` after changing dependencies (the flake checks it is current).
{
  pkgs,
  toolchain,
}:
let
  inherit (pkgs) lib;
  # Read by the desktop's build script (apps/desktop/build.rs).
  emojiAssets = import ./emoji.nix { inherit pkgs; };
  pkgConfig = [ pkgs.pkg-config ];
  # Libraries the final binaries link against (and whose -sys crates probe).
  linuxLibraries = [
    pkgs.alsa-lib
    pkgs.pipewire
    pkgs.fontconfig
    pkgs.freetype
    pkgs.libxkbcommon
    pkgs.wayland
    pkgs.libxcb
    pkgs.libx11
  ];
  withLibraries = _: {
    nativeBuildInputs = pkgConfig;
    buildInputs = linuxLibraries;
  };
  bindgen = _: {
    nativeBuildInputs = pkgConfig ++ [ pkgs.rustPlatform.bindgenHook ];
    buildInputs = [ pkgs.pipewire ];
  };
  overrides = pkgs.defaultCrateOverrides // {
    pipewire-sys = bindgen;
    libspa-sys = bindgen;
    # Their build scripts probe the PipeWire version with pkg-config.
    pipewire = bindgen;
    libspa = bindgen;
    yeslogic-fontconfig-sys = withLibraries;
    freetype-sys = withLibraries;
    wayland-sys = withLibraries;
    xkbcommon = withLibraries;
    # prost-build compiles the Mumble protocol definitions.
    nuschl-protocol = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };
    # The probe embeds the actual workspace fixture; isolated crate sources do
    # not contain ../../../tests. Keep identical frame bytes as an explicit input.
    nuschl-rtc-probe = _: {
      postPatch = ''
        substituteInPlace src/main.rs --replace-fail \
          '"../../../tests/fixtures/rtc-keyframe.vp8"' \
          '"${../tests/fixtures/rtc-keyframe.vp8}"'
      '';
    };
    # Its integration tests read Murmur fixtures from the workspace's
    # tests/fixtures, which the isolated crate source does not contain.
    nuschld = _: {
      postPatch = ''
        substituteInPlace tests/protocol.rs --replace-fail \
          '.join("../../tests/fixtures")' \
          '.join("${../tests/fixtures}")'
      '';
    };
    nuschl = attrs: withLibraries attrs // { NUSCHL_EMOJI_ASSETS = emojiAssets; };
    nuschl-media-worker = withLibraries;
    nuschl-cli = withLibraries;
  };
  project = import ./crate2nix/Cargo.nix {
    inherit pkgs;
    buildRustCrateForPkgs =
      cratePkgs:
      cratePkgs.buildRustCrate.override {
        rustc = toolchain;
        cargo = toolchain;
      };
    defaultCrateOverrides = overrides;
  };
in
{
  inherit project;
  # The programs of the desktop package (with the terminal client), each
  # built from cached crates.
  binaries = map (member: project.workspaceMembers.${member}.build) [
    "nuschl"
    "nuschl-cli"
    "nuschl-content-worker"
    "nuschl-crypto-worker"
    "nuschl-math-worker"
    "nuschl-media-worker"
    "nuschl-storage-worker"
    "nuschl-transfer-worker"
    "nuschl-tui"
    "nuschld"
    "nuschl-witness"
  ];
}
