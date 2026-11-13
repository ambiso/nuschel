# SPDX-License-Identifier: EUPL-1.2
# Actual native runtime and asset notices. No Rust compilation or new Cargo graph.
{
  pkgs,
  toolchain,
  projectRoot,
  webRuntimeNotices ? null,
  musicbotRuntimeNotices ? null,
}:
let
  cross = pkgs.pkgsCross.mingwW64;
  gcc = cross.stdenv.cc.cc;
  mingw = cross.windows.mingw_w64;
  native = {
    rust-standard-library =
      pkgs.runCommand "nuschl-rust-standard-library-notices" { nativeBuildInputs = [ pkgs.python3 ]; }
        ''
          python3 ${../release/native-notices.py} --component rust-standard-library \
            --rustc ${toolchain}/bin/rustc --rust-docs ${toolchain}/share/doc/rust --output "$out"
        '';
    gcc-runtime =
      pkgs.runCommand "nuschl-gcc-runtime-notices" { nativeBuildInputs = [ pkgs.python3 ]; }
        ''
          python3 ${../release/native-notices.py} --component gcc-runtime \
            --gcc-source ${gcc.src} --gcc-version ${gcc.version} --output "$out"
        '';
    mingw-w64-runtime =
      pkgs.runCommand "nuschl-mingw-runtime-notices" { nativeBuildInputs = [ pkgs.python3 ]; }
        ''
          python3 ${../release/native-notices.py} --component mingw-w64-runtime \
            --mingw-source ${mingw.src} --mingw-version ${mingw.version} --output "$out"
        '';
  };
  runtime = name: version: files: {
    inherit name version;
    source = "Exact pinned toolchain/source archive; see bundled ${name}/PROVENANCE.json and source hashes";
    files = map (file: {
      name = file;
      path = "${native.${name}}/${name}/${file}";
    }) files;
  };
  newcm = projectRoot + "/release/licenses/newcm";
  syntect = projectRoot + "/release/licenses/syntect-assets";
  syntax =
    builtins.head
      (builtins.fromJSON (builtins.readFile (syntect + "/provenance.json"))).external_components;
  syntaxAssets = builtins.fromJSON (builtins.readFile (syntect + "/ASSET-PROVENANCE.json"));
  font =
    builtins.head
      (builtins.fromJSON (builtins.readFile (newcm + "/provenance.json"))).external_components;
in
{
  inherit native;
  externalNotices =
    assert
      builtins.hashFile "sha256" (projectRoot + "/crates/math/fonts/NewCMMath-Regular.otf")
      == font.asset_sha256;
    [
      (runtime "rust-standard-library" toolchain.version [
        "NOTICES.txt"
        "PROVENANCE.json"
        "RUSTC.txt"
      ])
      (runtime "gcc-runtime" gcc.version [
        "NOTICES.txt"
        "PROVENANCE.json"
        "SOURCE-ARCHIVE-SHA256.txt"
      ])
      (runtime "mingw-w64-runtime" mingw.version [
        "NOTICES.txt"
        "PROVENANCE.json"
        "SOURCE-ARCHIVE-SHA256.txt"
      ])
      {
        name = "newcm-math-font";
        version = font.version;
        source = "${font.repository}/tree/${font.revision}/${font.asset}; SHA256 ${font.asset_sha256}";
        files = map (file: {
          name = file.local;
          path = "${newcm}/${file.local}";
          sha256 = file.sha256;
        }) font.files;
      }
      {
        name = "syntect-default-assets";
        version = syntax.version;
        source = "${syntax.repository}/tree/${syntax.revision}; exact default recipe/submodule grants retained";
        files = map (file: {
          name = file.local;
          path = "${syntect}/${file.local}";
          sha256 = file.sha256;
        }) syntax.files;
        assets = pkgs.lib.mapAttrsToList (name: sha256: {
          package = "syntect";
          version = syntax.version;
          path = "assets/${name}";
          inherit sha256;
        }) syntaxAssets.packed_assets_sha256;
      }
    ]
    ++ pkgs.lib.optional (webRuntimeNotices != null) {
      name = "web-runtime-assets";
      version = "locked-rendered-main-and-workers";
      source = "Actual Vite emitted npm owners, existing Temml ancestry and generated WASM/Cargo source identities";
      files =
        map
          (file: {
            name = file;
            path = "${webRuntimeNotices}/${file}";
          })
          [
            "NOTICES.txt"
            "BUNDLED-NPM.json"
            "WASM-PROVENANCE.json"
            "PACKAGE-LOCK.json"
          ];
    }
    ++ pkgs.lib.optional (musicbotRuntimeNotices != null) {
      name = "musicbot-web-runtime-assets";
      version = "locked-rendered-musicbot";
      source = "Actual musicbot Vite emitted npm owners and exact pinned npm source lock; no WASM or math worker";
      files =
        map
          (file: {
            name = file;
            path = "${musicbotRuntimeNotices}/${file}";
          })
          [
            "NOTICES.txt"
            "BUNDLED-NPM.json"
            "PACKAGE-LOCK.json"
            "EUPL-1.2"
          ];
    };
}
