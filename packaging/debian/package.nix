# Assemble exact Nix-built FHS bytes. This performs no signing or publication.
{
  pkgs,
  stage,
  projectRoot,
  version,
  maintainer,
  sourceDateEpoch,
  architecture ? "amd64",
  maximumGlibc ? "2.41",
}:
assert pkgs.lib.assertMsg (
  architecture == "amd64"
) "Only the measured amd64 native stage is currently supported";
pkgs.runCommand "nuschl-debian-${version}-${architecture}"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.dpkg
      pkgs.binutils
    ];
  }
  ''
    mkdir -p "$out"
    python3 ${./build-deb.py} --stage ${stage} --project-root ${projectRoot} \
      --architecture ${pkgs.lib.escapeShellArg architecture} \
      --maximum-glibc ${pkgs.lib.escapeShellArg maximumGlibc} \
      --version ${pkgs.lib.escapeShellArg version} --maintainer ${pkgs.lib.escapeShellArg maintainer} \
      --epoch ${toString sourceDateEpoch} --output "$out/nuschl_${version}_${architecture}.deb" \
      --readelf ${pkgs.binutils}/bin/readelf --dpkg-deb ${pkgs.dpkg}/bin/dpkg-deb \
      > "$out/validation.json"
    ${pkgs.dpkg}/bin/dpkg-deb --info "$out/nuschl_${version}_${architecture}.deb" > "$out/control.txt"
    cd "$out"
    sha256sum *.deb > SHA256SUMS
  ''
