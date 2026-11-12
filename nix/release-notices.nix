# SPDX-License-Identifier: EUPL-1.2
# Production scope comes from fixed package/platform call sites, never artifacts.
{
  pkgs,
  toolchain,
  craneLib,
  projectRoot,
  webRuntimeNotices,
  musicbotRuntimeNotices,
}:
let
  inherit (pkgs) lib;
  roots = builtins.fromJSON (builtins.readFile (projectRoot + "/release/notice-roots.json"));
  scopes = builtins.fromJSON (builtins.readFile (projectRoot + "/release/notice-scopes.json"));
  forScope =
    scope:
    let
      policy = scopes.${scope} or (throw "unknown release notice scope ${scope}");
      group = policy.root_group or "main";
      selectRoot =
        item:
        let
          standalone = item.standalone or { };
          selections =
            if group == "main" then
              [ item ]
            else if group == "all" then
              [ item ] ++ map (name: standalone.${name}) (builtins.attrNames standalone)
            else
              [ (standalone.${group} or (throw "missing standalone root group ${group}")) ];
        in
        {
          inherit (item) target;
          packages = lib.unique (lib.concatMap (selection: selection.packages) selections);
          features = lib.unique (lib.concatMap (selection: selection.features or [ ]) selections);
        };
      rootsByTarget = map selectRoot (
        builtins.filter (item: builtins.elem item.target policy.targets) roots
      );
      external = import ./release-external-notices.nix {
        inherit pkgs toolchain projectRoot;
        # Only products that actually ship a frontend depend on its notices.
        webRuntimeNotices =
          if builtins.elem "web-runtime-assets" policy.external_components then webRuntimeNotices else null;
        musicbotRuntimeNotices =
          if builtins.elem "musicbot-web-runtime-assets" policy.external_components then
            musicbotRuntimeNotices
          else
            null;
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
