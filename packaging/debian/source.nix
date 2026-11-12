# Genuine offline source upload; assembling it does not prove a distro rebuild.
{
  pkgs,
  craneLib,
  projectRoot,
  nativeNotices,
  desktopFiles,
  appstreamXML,
  version,
  sourceRevision,
  maintainer,
  sourceDateEpoch,
  distribution ? "unstable",
  debianRevision ? "1",
}:
assert builtins.match "[0-9][A-Za-z0-9.+~]*" version != null;
assert builtins.match "[0-9A-Za-z.+~]+" debianRevision != null;
assert builtins.match "[a-z][a-z0-9-]*" distribution != null;
assert builtins.match "[0-9a-f]{40}" sourceRevision != null;
assert !(pkgs.lib.hasInfix "\n" maintainer || pkgs.lib.hasInfix "\r" maintainer);
let
  vendor = craneLib.vendorCargoDeps { cargoLock = projectRoot + /Cargo.lock; };
in
pkgs.runCommand "nuschl-debian-source-${version}-${debianRevision}"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.dpkg
      pkgs.gnutar
      pkgs.xz
    ];
  }
  ''
      export SOURCE_DATE_EPOCH=${toString sourceDateEpoch}
      export TZ=UTC LC_ALL=C
      mkdir -p "$out" "nuschl-${version}"
      cp -a ${projectRoot}/. "nuschl-${version}/"
      chmod -R u+w "nuschl-${version}"
      cd "nuschl-${version}"
      test ! -e .git && test ! -e .jj && test ! -e target && test ! -e node_modules
      mkdir -p .cargo/registry-source
      cp -aL ${vendor}/*/. .cargo/registry-source/
      chmod -R u+w .cargo
      cat > .cargo/config.toml <<'EOF'
      [source.crates-io]
      replace-with = "offline-source"
      [source.offline-source]
      directory = ".cargo/registry-source"
      [net]
      offline = true
      EOF
      python3 ${./source-notices.py} --source "$PWD" --vendor "$PWD/.cargo/registry-source" \
        --output SOURCE-NOTICES.json > "$out/source-notice-summary.json"
      cat "$out/source-notice-summary.json"
      # This input is a frozen tracked VCS tree; no binaries/private caches are used.
      export NUSCHL_SOURCE_REVISION=${pkgs.lib.escapeShellArg sourceRevision}
      python3 - <<'PY'
      import json, os
      from pathlib import Path
      Path('SOURCE-PROVENANCE.json').write_text(json.dumps({
          'revision': os.environ['NUSCHL_SOURCE_REVISION'],
          'scope': 'Genuine source build; external distro/Launchpad binaries have separate provenance from Nix final-leaf relinks.',
          'publication_gates': ['Review all SOURCE-NOTICES unresolved rows/embedded assets.',
                                'Supply/test Rust >=1.95 build dependencies for the chosen suite.',
                                'Perform a clean target source rebuild and sandbox/runtime tests.'],
      }, indent=2) + '\n')
      PY
      cd ..
      tar --sort=name --mtime=@${toString sourceDateEpoch} --owner=0 --group=0 --numeric-owner \
        --mode='u=rwX,go=rX' -c "nuschl-${version}" | xz --threads=2 -3 > "$out/nuschl_${version}.orig.tar.xz"
      cd "nuschl-${version}"
      mkdir -p debian/source debian/prepared-desktop/share/metainfo debian/prepared-notices
      echo '3.0 (quilt)' > debian/source/format
      cp ${./rules} debian/rules
      chmod 755 debian/rules
      cp ${./control.in} debian/control
      chmod u+w debian/control
      cp ${projectRoot}/LICENSE debian/copyright
      cp -a ${desktopFiles}/share/. debian/prepared-desktop/share/
      cp ${appstreamXML} debian/prepared-desktop/share/metainfo/org.nuschl.Nuschl.metainfo.xml
      cp -a ${nativeNotices}/share/licenses/nuschl/. debian/prepared-notices/
      export NUSCHL_MAINTAINER=${pkgs.lib.escapeShellArg maintainer}
      python3 - <<'PY'
      import email.utils, os
      from pathlib import Path
      # Only prepared original icons and required corresponding-source archives
    # belong in the Debian patch tar. No prebuilt application/helper is included.
    binaries = []
    for path in Path('debian/prepared-desktop/share/icons/hicolor').glob('*/apps/org.nuschl.Nuschl.png'):
        if path.parts[-3] not in {'16x16', '24x24', '32x32', '48x48', '64x64', '128x128', '256x256', '512x512'}:
            raise ValueError('unexpected prepared icon size')
        binaries.append(str(path))
    binaries.extend(str(path) for path in Path('debian/prepared-notices').glob('*-source.tar'))
    Path('debian/source/include-binaries').write_text('\n'.join(sorted(binaries)) + '\n')
    maintainer = os.environ['NUSCHL_MAINTAINER']
      path = Path('debian/control')
      path.write_text(path.read_text().replace('@MAINTAINER@', maintainer))
      date = email.utils.formatdate(int(os.environ['SOURCE_DATE_EPOCH']), usegmt=True)
      Path('debian/changelog').write_text(
          'nuschl (${version}-${debianRevision}) ${distribution}; urgency=medium\n\n'
          '  * Local unsigned offline source packaging validation.\n\n'
          f' -- {maintainer}  {date}\n')
      PY
      # dpkg-source expects the orig next to the source directory.
      ln -s "$out/nuschl_${version}.orig.tar.xz" ../nuschl_${version}.orig.tar.xz
      dpkg-source --build .
      dpkg-genchanges --build=source -sa > "$out/nuschl_${version}-${debianRevision}_source.changes"
      cp ../nuschl_${version}-${debianRevision}.dsc ../nuschl_${version}-${debianRevision}.debian.tar.* "$out/"
      cp SOURCE-NOTICES.json SOURCE-PROVENANCE.json "$out/"
      echo 'LOCAL UNSIGNED VALIDATION ONLY: source rebuild/compiler/source-license gates remain.' > "$out/STATUS"
      cd "$out"
      sha256sum *.dsc *.tar.* *_source.changes > SHA256SUMS
  ''
