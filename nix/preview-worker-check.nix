# SPDX-License-Identifier: EUPL-1.2
# Compile the harness=false fixture against the actual cached IPC rlibs, then
# exercise the production worker and hostile children through real confinement.
{
  pkgs,
  toolchain,
  ipc,
  contentWorker,
}:
let
  inherit (pkgs) lib;
  fixture = ../crates/ipc/tests/preview.rs;
  workspace = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace;
  extern =
    dep:
    let
      name = lib.replaceStrings [ "-" ] [ "_" ] dep.libName;
      extension =
        if lib.any (type: type == "lib" || type == "rlib") dep.crateType then
          ".rlib"
        else
          pkgs.stdenv.hostPlatform.extensions.library;
    in
    "${name}=${lib.getLib dep}/lib/lib${name}-${dep.metadata}${extension}";
  dependencies = [ ipc ] ++ ipc.dependencies;
  dirs = map (dep: "${lib.getLib dep}/lib") (lib.unique ([ ipc ] ++ ipc.completeDeps));
  lintFlags = {
    forbid = "-F";
    deny = "-D";
    warn = "-W";
    allow = "-A";
  };
  lints = lib.mapAttrsToList (
    name: setting:
    "${lintFlags.${if builtins.isAttrs setting then setting.level else setting}} ${name}"
  ) (workspace.lints.rust or { });
  diagnostic = pkgs.runCommandCC "nuschl-preview-worker-diagnostic" {
    nativeBuildInputs = [ toolchain ];
  } ''
    links=()
    for dir in ${lib.concatStringsSep " " dirs}; do
      if [ -s "$dir/link" ]; then
        while read -r flag; do links+=($flag); done < "$dir/link"
      fi
    done
    mkdir -p "$out/bin"
    rustc --edition ${workspace.package.edition} --crate-name preview --crate-type bin \
      ${fixture} -o "$out/bin/preview" \
      -C opt-level=0 -C debug-assertions=on -C overflow-checks=on \
      -C linker=${pkgs.stdenv.cc}/bin/${pkgs.stdenv.cc.targetPrefix}cc \
      ${lib.concatStringsSep " " lints} \
      ${lib.concatMapStringsSep " " (dep: "--extern ${extern dep}") dependencies} \
      ${lib.concatMapStringsSep " " (dir: "-L dependency=${dir}") dirs} \
      "''${links[@]}"
  '';
in
pkgs.runCommand "nuschl-preview-worker-check" {
  nativeBuildInputs = [
    pkgs.bubblewrap
    pkgs.util-linux
    pkgs.coreutils
  ];
  passthru = { inherit diagnostic; };
} ''
  mkdir -p "$out"
  timeout --kill-after=5 30 \
    ${diagnostic}/bin/preview ${contentWorker}/bin/nuschl-content-worker \
    > "$out/stdout" 2> "$out/stderr"
  grep -qxF 'preview: bounded confined replies, cancellation retirement and recovery ... ok' "$out/stdout"
''
