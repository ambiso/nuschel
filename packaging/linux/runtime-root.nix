# Test userspace only; none of these native libraries/tools are bundled in .deb.
{ pkgs, sysroot }:
let
  pins = builtins.fromJSON (builtins.readFile ./debian/runtime-lock.json);
  base = builtins.fromJSON (builtins.readFile ./debian/sysroot-lock.json);
  extra = builtins.filter (row: !(builtins.elem row.name (map (p: p.name) base.packages))) pins.packages;
  archives = map (row: pkgs.fetchurl {
    url = "https://deb.debian.org/debian/${row.filename}";
    sha256 = row.sha256;
  }) extra;
  index = pkgs.fetchurl { url = pins.packages_url; sha256 = pins.packages_sha256; };
  check = pkgs.runCommand "nuschl-native-runtime-metadata" { nativeBuildInputs = [ pkgs.python3 pkgs.gnupg ]; } ''
    mkdir scripts metadata
    cp ${./pin-runtime.py} scripts/pin-runtime.py
    cp ${./pin-debian.py} scripts/pin-debian.py
    cp ${./debian/Release} metadata/Release
    cp ${./debian/Release.gpg} metadata/Release.gpg
    python3 scripts/pin-runtime.py --release metadata/Release --signature metadata/Release.gpg \
      --packages ${index} --keyring ${pkgs.debian-archive-keyring}/share/keyrings/debian-archive-keyring.gpg \
      --gpgv ${pkgs.gnupg}/bin/gpgv --output checked.json
    cmp checked.json ${./debian/runtime-lock.json}
    touch "$out"
  '';
in pkgs.runCommand "nuschl-debian13-native-test-root" { nativeBuildInputs = [ pkgs.dpkg pkgs.python3 ]; } ''
  test -e ${check}
  cp -a ${sysroot} "$out"
  chmod -R u+w "$out"
  python3 ${./extract-deb.py} --dpkg-deb ${pkgs.dpkg}/bin/dpkg-deb --root "$out" \
    ${pkgs.lib.escapeShellArgs archives}
  chmod -R u+w,a-s "$out"
  export NUSCHL_ROOT="$out"
  python3 - <<'PY'
  import os
  from pathlib import Path
  root = Path(os.environ['NUSCHL_ROOT'])
  for path in root.rglob('*'):
      if path.is_symlink() and (target := os.readlink(path)).startswith('/'):
          path.unlink()
          path.symlink_to(os.path.relpath(root / target.lstrip('/'), path.parent))
  PY
''
