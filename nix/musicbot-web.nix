# The music bot's Svelte UI, built independently of the Rust binary.
{ pkgs, lib }:
pkgs.buildNpmPackage {
  pname = "nuschl-musicbot-web";
  version = "0.1.0";
  # Keep the same tracked relative helper imports in Nix and local Vite builds.
  src = lib.cleanSourceWith {
    src = ../.;
    name = "source";
    filter =
      path: type:
      let
        relative = lib.removePrefix (toString ../. + "/") (toString path);
      in
      !(builtins.elem (builtins.baseNameOf path) [
        "node_modules"
        "dist"
      ])
      && (
        type == "directory"
        || lib.hasPrefix "apps/musicbot/web/" relative
        || builtins.elem relative [
          "web/app/build-runtime-notices.js"
          "release/web-notices.py"
          "LICENSE"
        ]
      );
  };
  sourceRoot = "source/apps/musicbot/web";
  nativeBuildInputs = [ pkgs.python3 ];
  npmDepsHash = "sha256-09jSgvibSrdH9y/CYzIsOgXg3/yJOEIur/porGJKa+Y=";
  doCheck = true;
  checkPhase = "npm test";
  installPhase = ''
    mkdir -p "$out"
    cp -R dist/. "$out/"
  '';
}
