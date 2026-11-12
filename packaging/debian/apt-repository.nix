# Local unsigned APT tree for exact Nix-built .deb files; no keys or publication.
{
  pkgs,
  packages,
  suite,
  publicationEpoch,
}:
assert builtins.match "[a-z][a-z0-9.-]*" suite != null;
assert builtins.isInt publicationEpoch && publicationEpoch >= 0;
assert packages != [ ];
pkgs.runCommand "nuschl-apt-${suite}"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.dpkg
      pkgs.gzip
    ];
  }
  ''
    mkdir -p "$out/pool/main/n/nuschl" "$out/dists/${suite}/main/binary-amd64"
    for package in ${pkgs.lib.escapeShellArgs packages}; do
      test "$(dpkg-deb -f "$package" Package)" = nuschl
      test "$(dpkg-deb -f "$package" Architecture)" = amd64
      cp "$package" "$out/pool/main/n/nuschl/"
    done
    cd "$out"
    dpkg-scanpackages --arch amd64 pool /dev/null > "dists/${suite}/main/binary-amd64/Packages"
    gzip -9 -n -c "dists/${suite}/main/binary-amd64/Packages" > "dists/${suite}/main/binary-amd64/Packages.gz"
    python3 ${./apt-release.py} --root "$out" --suite ${pkgs.lib.escapeShellArg suite} --epoch ${toString publicationEpoch}
    echo 'UNSIGNED LOCAL VALIDATION. Do not enroll without operator signing, provenance/license review and clean distro tests.' > STATUS
  ''
