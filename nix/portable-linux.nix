# Native Linux package prototype: only final leaves relink, Rust deps stay cached.
# The sysroot consists of exact SHA256-pinned official Debian 13 packages whose
# metadata chain is checked independently. Extracting it never runs maintainer scripts.
{
  pkgs,
  toolchain,
  projectRoot ? ../.,
  nativeNotices ? null,
  desktopFiles ? (import ./desktop-files.nix { inherit pkgs; }).linux,
  appstreamXML ? null,
  revision ? null,
  sourceNarHash ? null,
  assetBinding ? null,
  version ? "0.1.0",
}:
assert pkgs.lib.assertMsg (
  pkgs.stdenv.hostPlatform.system == "x86_64-linux"
) "The signed native sysroot currently targets amd64 Linux only";
let
  inherit (pkgs) lib;
  packagingSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ./portable-linux.nix
      ../packaging/debian/build-deb.py
      ../packaging/linux/audit-stage.py
      ../packaging/linux/default.nix
      ../packaging/linux/version.nix
      ../packaging/linux/bind-assets.py
      ../packaging/linux/linker.py
      ../packaging/linux/pin-debian.py
      ../packaging/linux/debian/sysroot-lock.json
    ];
  };
  pins = builtins.fromJSON (builtins.readFile ../packaging/linux/debian/sysroot-lock.json);
  assertPins =
    assert pins.schema == 1 && pins.architecture == "amd64";
    true;
  index = pkgs.fetchurl {
    url = pins.packages_url;
    sha256 = pins.packages_sha256;
  };
  archives = map (
    row:
    pkgs.fetchurl {
      url = "https://deb.debian.org/debian/${row.filename}";
      sha256 = row.sha256;
    }
  ) pins.packages;
  metadataCheck =
    pkgs.runCommand "nuschl-debian-sysroot-metadata"
      {
        nativeBuildInputs = [
          pkgs.python3
          pkgs.gnupg
        ];
      }
      ''
        cp ${../packaging/linux/debian/Release} Release
        cp ${../packaging/linux/debian/Release.gpg} Release.gpg
        python3 ${../packaging/linux/pin-debian.py} \
          --release "$PWD/Release" --signature "$PWD/Release.gpg" \
          --packages ${index} \
          --keyring ${pkgs.debian-archive-keyring}/share/keyrings/debian-archive-keyring.gpg \
          --output computed.json
        cmp computed.json ${../packaging/linux/debian/sysroot-lock.json}
        touch "$out"
      '';
  sysroot =
    assert assertPins;
    pkgs.runCommand "nuschl-debian13-amd64-sysroot"
      {
        nativeBuildInputs = [
          pkgs.dpkg
          pkgs.python3
        ];
      }
      ''
        test -e ${metadataCheck}
        mkdir -p "$out"
        for archive in ${lib.escapeShellArgs archives}; do
          dpkg-deb --extract "$archive" "$out"
        done
        chmod -R u+w,a-s "$out"
        test -e "$out/usr/lib/x86_64-linux-gnu/libc.so.6"
        if test ! -e "$out/lib"; then ln -s usr/lib "$out/lib"; fi
        if test ! -e "$out/lib64"; then ln -s usr/lib64 "$out/lib64"; fi
        # Absolute symlinks in a target filesystem must resolve inside the sysroot,
        # rather than following host /usr while the linker opens a library.
        export NUSCHL_SYSROOT="$out"
        python3 - <<'PY'
        import os
        import re
        from pathlib import Path
        root = Path(os.environ['NUSCHL_SYSROOT'])
        for path in root.rglob('*'):
            if path.is_symlink():
                target = os.readlink(path)
                if target.startswith('/'):
                    path.unlink()
                    path.symlink_to(os.path.relpath(root / target.lstrip('/'), path.parent))
        # Anchor trusted GNU ld scripts to this immutable build sysroot.
        # Final shared-library SONAMEs remain unchanged.
        for path in (root / 'usr/lib').rglob('*.so'):
            if path.is_symlink() or not path.is_file():
                continue
            with path.open('rb') as stream:
                marker = stream.read(32)
            if marker.startswith(b'/* GNU ld script'):
                if path.stat().st_size > 65536:
                    raise ValueError('oversized linker script')
                text = path.read_text()
                text = re.sub(r'(?<![A-Za-z0-9_])/(?:usr/)?lib(?:64)?/', lambda match: str(root) + match[0], text)
                path.write_text(text)
        PY
      '';
  linker = pkgs.writeShellScriptBin "nuschl-fhs-cc" ''
    exec ${pkgs.python3}/bin/python3 ${../packaging/linux/linker.py} \
      ${sysroot} ${pkgs.gcc.cc}/bin/gcc "$@"
  '';
  crates = import (projectRoot + "/nix/crate2nix.nix") { inherit pkgs toolchain; };
  leafWith =
    name: options:
    (crates.project.workspaceMembers.${name}.build.override options).overrideAttrs (old: {
      preBuild = (old.preBuild or "") + ''
        export NUSCHL_LINK_REPORT="$PWD/native-link.json"
        export EXTRA_RUSTC_FLAGS="''${EXTRA_RUSTC_FLAGS:-} -C linker=${linker}/bin/nuschl-fhs-cc"
      '';
      postInstall = (old.postInstall or "") + ''
        install -Dm644 "$NUSCHL_LINK_REPORT" "$out/nix-support/native-link.json"
      '';
      dontFixup = true;
    });
  leaf = name: leafWith name { };
  programs = {
    nuschl = leaf "nuschl";
    nuschl-content-worker = leaf "nuschl-content-worker";
    nuschl-crypto-worker = leaf "nuschl-crypto-worker";
    nuschl-math-worker = leaf "nuschl-math-worker";
    nuschl-media-worker = leaf "nuschl-media-worker";
    nuschl-storage-worker = leaf "nuschl-storage-worker";
    nuschl-transfer-worker = leaf "nuschl-transfer-worker";
    nuschl-update-verify-ed25519 = leafWith "nuschl-update-worker" { features = [ "ed25519" ]; };
    nuschl-update-verify-mldsa44 = leafWith "nuschl-update-worker" { features = [ "mldsa44" ]; };
  };
  stage =
    assert nativeNotices != null;
    assert
      (revision != null && builtins.match "[0-9a-f]{40}" revision != null)
      || (sourceNarHash != null && builtins.match "sha256-[A-Za-z0-9+/]{43}=" sourceNarHash != null);
    pkgs.runCommand (lib.strings.sanitizeDerivationName "nuschl-native-fhs-${version}")
      {
        nativeBuildInputs = [
          pkgs.python3
          pkgs.binutils
        ];
      }
      ''
        ${lib.optionalString (assetBinding != null) "test -s ${assetBinding}"}
        mkdir -p "$out/usr/lib/nuschl" "$out/usr/bin" "$out/usr/share/nuschl/link-receipts"
        ${lib.concatStringsSep "\n" (
          lib.mapAttrsToList (name: program: ''
            install -Dm755 ${program}/bin/${name} "$out/usr/lib/nuschl/${name}"
            install -Dm644 ${program}/nix-support/native-link.json "$out/usr/share/nuschl/link-receipts/${name}.json"
          '') programs
        )}
        ln -s ../lib/nuschl/nuschl "$out/usr/bin/nuschl"
        cp -R ${desktopFiles}/share/. "$out/usr/share/"
        mkdir -p "$out/usr/share/licenses/nuschl"
        cp -R ${nativeNotices}/share/licenses/nuschl/. "$out/usr/share/licenses/nuschl/"
        install -Dm644 ${projectRoot}/LICENSE "$out/usr/share/licenses/nuschl/EUPL-1.2"
        install -Dm644 ${projectRoot}/crates/protocol/proto/LICENSE.mumble "$out/usr/share/licenses/nuschl/Mumble-BSD"
        ${lib.optionalString (appstreamXML != null) ''
          mkdir -p "$out/usr/share/metainfo"
          install -Dm644 ${appstreamXML} "$out/usr/share/metainfo/org.nuschl.Nuschl.metainfo.xml"
        ''}
        ${lib.optionalString (assetBinding != null) ''
          install -Dm644 ${assetBinding} "$out/usr/share/nuschl/SOURCE-ASSETS.json"
        ''}
        python3 ${../packaging/linux/audit-stage.py} --stage "$out" \
          --project-root ${projectRoot} --packaging-root ${packagingSource} \
          ${lib.optionalString (revision != null) "--revision ${lib.escapeShellArg revision}"} \
          ${
            lib.optionalString (sourceNarHash != null) "--source-nar-hash ${lib.escapeShellArg sourceNarHash}"
          } \
          --version ${lib.escapeShellArg version} --readelf ${pkgs.binutils}/bin/readelf \
          --rust-version ${lib.escapeShellArg toolchain.version}
      '';

in
{
  inherit
    metadataCheck
    sysroot
    linker
    programs
    stage
    ;
  tests =
    pkgs.runCommand "nuschl-native-packaging-tests"
      {
        nativeBuildInputs = [
          pkgs.python3
          pkgs.binutils
          pkgs.patchelf
          toolchain
        ];
      }
      ''
            mkdir -p packaging/linux packaging/debian
        cp ${../packaging/linux/tests.py} packaging/linux/tests.py
        cp ${../packaging/linux/linker.py} packaging/linux/linker.py
        cp ${../packaging/debian/build-deb.py} packaging/debian/build-deb.py
        python3 packaging/linux/tests.py \
              --scratch "$TMPDIR" --rustc ${toolchain}/bin/rustc \
              --readelf ${pkgs.binutils}/bin/readelf --patchelf ${pkgs.patchelf}/bin/patchelf
            touch "$out"
      '';
  content = programs.nuschl-content-worker;
  desktop = programs.nuschl;
}
