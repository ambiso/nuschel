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
  metadataArgs = pkgs.lib.escapeShellArgs [
    "--revision"
    sourceRevision
    "--version"
    version
    "--debian-revision"
    debianRevision
    "--distribution"
    distribution
    "--maintainer"
    maintainer
    "--epoch"
    (toString sourceDateEpoch)
  ];
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
    python3 ${./prepare-source.py} --phase upstream ${metadataArgs}
    python3 ${./source-notices.py} --source "$PWD" --vendor "$PWD/.cargo/registry-source" \
      --output SOURCE-NOTICES.json > "$out/source-notice-summary.json"
    cat "$out/source-notice-summary.json"
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
    chmod -R u+w debian
    python3 ${./prepare-source.py} --phase debian ${metadataArgs}
    ln -s "$out/nuschl_${version}.orig.tar.xz" ../nuschl_${version}.orig.tar.xz
    dpkg-source --build .
    dpkg-genchanges --build=source -sa > "$out/nuschl_${version}-${debianRevision}_source.changes"
    cp ../nuschl_${version}-${debianRevision}.dsc ../nuschl_${version}-${debianRevision}.debian.tar.* "$out/"
    cp SOURCE-NOTICES.json SOURCE-PROVENANCE.json "$out/"
    echo 'LOCAL UNSIGNED VALIDATION ONLY: source rebuild/compiler/source-license gates remain.' > "$out/STATUS"
    test -s "$out/nuschl_${version}-${debianRevision}.dsc"
    test -s "$out/nuschl_${version}-${debianRevision}_source.changes"
    test -s "$out/nuschl_${version}.orig.tar.xz"
    cd "$out"
    sha256sum *.dsc *.tar.* *_source.changes > SHA256SUMS
    timeout 180 python3 ${./validate-source.py} --root "$out" --version ${pkgs.lib.escapeShellArg version} \
      --debian-revision ${pkgs.lib.escapeShellArg debianRevision} --output "$out/validation.json"
  ''
