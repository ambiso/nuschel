# SPDX-License-Identifier: EUPL-1.2
# Production scope comes from fixed package/platform call sites, never artifacts.
{
  pkgs,
  toolchain,
  craneLib,
  projectRoot,
  webRuntimeNotices,
}:
let
  roots = builtins.fromJSON (builtins.readFile (projectRoot + "/release/notice-roots.json"));
  scopes = builtins.fromJSON (builtins.readFile (projectRoot + "/release/notice-scopes.json"));
  forScope =
    scope:
    let
      policy = scopes.${scope} or (throw "unknown release notice scope ${scope}");
      rootsByTarget = builtins.filter (item: builtins.elem item.target policy.targets) roots;
      external = import ./release-external-notices.nix {
        inherit pkgs toolchain projectRoot;
        # A native package must not gain a web/NPM build dependency for notices.
        webRuntimeNotices =
          if builtins.elem "web-runtime-assets" policy.external_components then webRuntimeNotices else null;
      };
      externalNotices = builtins.filter (
        item: builtins.elem item.name policy.external_components
      ) external.externalNotices;
    in
    assert builtins.length rootsByTarget == builtins.length policy.targets;
    assert
      builtins.sort builtins.lessThan (map (item: item.target) rootsByTarget)
      == builtins.sort builtins.lessThan policy.targets;
    assert builtins.length externalNotices == builtins.length policy.external_components;
    import ./release-licenses.nix {
      inherit
        pkgs
        craneLib
        projectRoot
        rootsByTarget
        externalNotices
        scope
        ;
    };
in
{
  inherit forScope;
  all = forScope "all";
  linux = forScope "linux-nix";
  windows = forScope "windows-x86_64";
  macos = forScope "macos-universal";
  web = forScope "web";
}
