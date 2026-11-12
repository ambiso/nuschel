{ pkgs, stage, version, sourceDateEpoch, sourceURL ? null }:
assert pkgs.lib.assertMsg (builtins.match "[0-9][A-Za-z0-9._+]*" version != null) "Invalid Arch version";
pkgs.runCommand "nuschl-arch-artifacts-${version}" {
  nativeBuildInputs = [ pkgs.python3 pkgs.gnutar pkgs.xz pkgs.pacman pkgs.fakeroot pkgs.file pkgs.libarchive pkgs.zstd ];
} ''
  export SOURCE_DATE_EPOCH=${toString sourceDateEpoch}
  mkdir -p "$out"
  tar --sort=name --mtime=@${toString sourceDateEpoch} --owner=0 --group=0 --numeric-owner \
    --mode='u=rwX,go=rX' -C ${stage} -c usr | xz --threads=2 > "$out/nuschl-${version}-x86_64.tar.xz"
  python3 ${./generate.py} --archive "$out/nuschl-${version}-x86_64.tar.xz" \
    --version ${pkgs.lib.escapeShellArg version} --output "$out/aur" \
    ${pkgs.lib.optionalString (sourceURL != null) "--source-url ${pkgs.lib.escapeShellArg sourceURL}"}
  cd "$out/aur"
  makepkg --config ${pkgs.pacman}/etc/makepkg.conf --printsrcinfo > .SRCINFO
  # Test the exact recipe/package() and checksum with makepkg itself. --nodeps
  # is necessary in the Nix build environment, not an Arch install exemption.
  cp ${pkgs.pacman}/etc/makepkg.conf makepkg.conf
  chmod u+w makepkg.conf
  printf '%s\n' 'COMPRESSZST=(zstd -c -T2 -9 -)' >> makepkg.conf
  export SRCDEST="$out" PKGDEST="$out/packages" BUILDDIR="$TMPDIR/makepkg"
  export PACKAGER='Local package validation <nobody@example.invalid>'
  mkdir -p "$PKGDEST" "$BUILDDIR"
  makepkg --config "$PWD/makepkg.conf" --nodeps --nocheck
  rm makepkg.conf
  cd "$out"
  sha256sum *.tar.xz packages/*.pkg.tar.zst > SHA256SUMS
''
