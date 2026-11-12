# Native Linux package prototype: only final leaves relink, Rust deps stay cached.
# The sysroot consists of exact SHA256-pinned official Debian 13 packages whose
# metadata chain is checked independently. Extracting it never runs maintainer scripts.
{
  pkgs,
  toolchain,
  projectRoot ? ../.,
}:
let
  inherit (pkgs) lib;
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
  crates = import (projectRoot + /nix/crate2nix.nix) { inherit pkgs toolchain; };
  leaf =
    name:
    crates.project.workspaceMembers.${name}.build.overrideAttrs (old: {
      preBuild = (old.preBuild or "") + ''
        export EXTRA_RUSTC_FLAGS="''${EXTRA_RUSTC_FLAGS:-} -C linker=${linker}/bin/nuschl-fhs-cc"
      '';
      # Nix's normal ELF fixup would restore a store interpreter/RPATH. This
      # prototype instead validates the resulting target ABI explicitly.
      dontFixup = true;
    });
in
{
  inherit metadataCheck sysroot linker;
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
  content = leaf "nuschl-content-worker";
  desktop = leaf "nuschl";
}
