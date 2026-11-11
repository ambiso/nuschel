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
    nuschel-protocol = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };
    nuschel = withLibraries;
    nuschel-media-worker = withLibraries;
    nuschel-cli = withLibraries;
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
  # The programs of the desktop package, each built from cached crates.
  binaries = map (member: project.workspaceMembers.${member}.build) [
    "nuschel"
    "nuschel-cli"
    "nuschel-content-worker"
    "nuschel-crypto-worker"
    "nuschel-math-worker"
    "nuschel-media-worker"
    "nuschel-storage-worker"
    "nuschel-transfer-worker"
    "nuscheld"
  ];
}
