{ pkgs, stage, version, sourceDateEpoch, sourceURL ? null }:
assert pkgs.lib.assertMsg (builtins.match "[0-9][A-Za-z0-9._+]*" version != null) "Invalid Arch version";
pkgs.runCommand "nuschl-arch-artifacts-${version}" {
  nativeBuildInputs = [ pkgs.python3 pkgs.gnutar pkgs.xz pkgs.pacman ];
} ''
  mkdir -p "$out"
  tar --sort=name --mtime=@${toString sourceDateEpoch} --owner=0 --group=0 --numeric-owner \
    --mode='u=rwX,go=rX' -C ${stage} -c usr | xz --threads=2 > "$out/nuschl-${version}-x86_64.tar.xz"
  python3 ${./generate.py} --archive "$out/nuschl-${version}-x86_64.tar.xz" \
    --version ${pkgs.lib.escapeShellArg version} --output "$out/aur" \
    ${pkgs.lib.optionalString (sourceURL != null) "--source-url ${pkgs.lib.escapeShellArg sourceURL}"}
  cd "$out/aur"
  makepkg --config ${pkgs.pacman}/etc/makepkg.conf --printsrcinfo > .SRCINFO
  cd "$out"
  sha256sum *.tar.xz > SHA256SUMS
''
