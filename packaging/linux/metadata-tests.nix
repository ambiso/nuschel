# Cheap offline packaging-policy fixtures, without a Cargo dependency compile.
{ pkgs }:
let
  versionCheck = import ./version-tests.nix { inherit pkgs; };
in
pkgs.runCommand "nuschl-distro-metadata-tests"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.gitMinimal
      pkgs.dpkg
      pkgs.pacman
    ];
  }
  ''
    test -e ${versionCheck}
    mkdir -p packaging/linux packaging/debian
    cp ${./version.py} packaging/linux/version.py
    cp ${./version-tests.py} packaging/linux/version-tests.py
    cp ${./extract-deb.py} packaging/linux/extract-deb.py
    cp ${./extract-tests.py} packaging/linux/extract-tests.py
    cp ${../debian/prepare-source.py} packaging/debian/prepare-source.py
    cp ${../debian/validate-source.py} packaging/debian/validate-source.py
    cp ${../debian/source-tests.py} packaging/debian/source-tests.py
    python3 packaging/linux/version-tests.py --dpkg ${pkgs.dpkg}/bin/dpkg --vercmp ${pkgs.pacman}/bin/vercmp
    python3 packaging/linux/extract-tests.py
    python3 packaging/debian/source-tests.py --dpkg-parsechangelog ${pkgs.dpkg}/bin/dpkg-parsechangelog
    touch "$out"
  ''
