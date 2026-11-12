# Pure public builder entrypoint. The coordinator owns flake output registration.
{
  pkgs,
  toolchain,
  craneLib,
  sourceInfo,
  versionMetadata ? null,
  nativeNotices,
  desktopFiles,
  appstreamXML,
  maintainer ? "Local package validation <nobody@example.invalid>",
}:
let
  inherit (pkgs) lib;
  projectRoot = sourceInfo.outPath;
  baseVersion =
    (builtins.fromTOML (builtins.readFile (projectRoot + "/Cargo.toml"))).workspace.package.version;
  versions = import ./version.nix {
    inherit baseVersion sourceInfo;
    metadata = versionMetadata;
  };
  expectedNotices =
    (import (projectRoot + "/nix/release-notices.nix") {
      inherit
        pkgs
        toolchain
        craneLib
        projectRoot
        ;
      webRuntimeNotices = null;
    }).linux.bundle;
  expectedDesktop = (import (projectRoot + "/nix/desktop-files.nix") { inherit pkgs; }).linux;
  assetBinding =
    pkgs.runCommand "nuschl-native-source-assets" { nativeBuildInputs = [ pkgs.python3 ]; }
      ''
        python3 ${./bind-assets.py} --source ${projectRoot} --notices ${nativeNotices} \
          --expected-notices ${expectedNotices} --desktop ${desktopFiles} --expected-desktop ${expectedDesktop} \
          --source-nar-hash ${lib.escapeShellArg versions.sourceNarHash} \
          ${
            lib.optionalString (
              versions.sourceRevision != null
            ) "--revision ${lib.escapeShellArg versions.sourceRevision}"
          } \
          ${lib.optionalString (appstreamXML != null) "--appstream ${appstreamXML}"} --output "$out"
      '';
  portable = import ../../nix/portable-linux.nix {
    inherit
      pkgs
      toolchain
      projectRoot
      nativeNotices
      desktopFiles
      appstreamXML
      assetBinding
      ;
    revision = versions.sourceRevision;
    sourceNarHash = versions.sourceNarHash;
    version = versions.debianUpstream;
  };
  deb = import ../debian/package.nix {
    inherit pkgs projectRoot maintainer;
    stage = portable.stage;
    version = versions.debianVersion;
    sourceDateEpoch = versions.sourceDateEpoch;
  };
  archBin = import ../arch/package.nix {
    inherit pkgs;
    stage = portable.stage;
    version = versions.archPkgver;
    packageRevision = versions.archPkgrel;
    sourceDateEpoch = versions.sourceDateEpoch;
  };
  debSource = import ../debian/source.nix {
    inherit
      pkgs
      craneLib
      projectRoot
      nativeNotices
      desktopFiles
      appstreamXML
      maintainer
      assetBinding
      ;
    version = versions.debianUpstream;
    sourceRevision = versions.sourceRevision;
    sourceNarHash = versions.sourceNarHash;
    sourceDateEpoch = versions.sourceDateEpoch;
    debianRevision = versions.packageRevision;
  };
in
{
  inherit
    versions
    deb
    archBin
    debSource
    assetBinding
    ;
  nativeStage = portable.stage;
  debSourceCheck = import ../debian/check-source.nix {
    inherit pkgs toolchain;
    sourcePackage = debSource;
    version = versions.debianUpstream;
    debianRevision = versions.packageRevision;
  };
  metadataTests = import ./metadata-tests.nix { inherit pkgs; };
}
// lib.optionalAttrs versions.officialVersion {
  aptRepository = import ../debian/apt-repository.nix {
    inherit pkgs;
    packages = [ (deb + "/nuschl_${versions.debianVersion}_amd64.deb") ];
    suite = "trixie";
    publicationEpoch = versions.sourceDateEpoch;
  };
}
