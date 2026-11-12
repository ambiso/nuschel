# Pure sidecar/type/binding failures plus actual distro version comparators.
{ pkgs }:
let
  mk = import ./version.nix;
  nar = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
  otherNar = "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";
  revision = "1111111111111111111111111111111111111111";
  info = {
    narHash = nar;
    rev = revision;
  };
  meta = {
    schema = 1;
    kind = "development";
    base_version = "0.1.0";
    source_nar_hash = nar;
    source_revision = revision;
    sequence = 9;
    commit_epoch = 1794520000;
    commit_stamp = "20261112214640";
    tag = null;
    package_revision = 1;
  };
  call =
    metadata:
    mk {
      baseVersion = "0.1.0";
      sourceInfo = info;
      inherit metadata;
    };
  rejected = metadata: !(builtins.tryEval (builtins.deepSeq (call metadata) true)).success;
  invalid = [
    (meta // { schema = 2; })
    (meta // { unused = true; })
    (builtins.removeAttrs meta [ "tag" ])
    (meta // { kind = "unknown"; })
    (meta // { base_version = "0.2.0"; })
    (meta // { source_nar_hash = otherNar; })
    (meta // { source_revision = "2222222222222222222222222222222222222222"; })
    (meta // { sequence = 0; })
    (meta // { sequence = 1.5; })
    (meta // { sequence = "10"; })
    (meta // { commit_epoch = -1; })
    (meta // { commit_epoch = true; })
    (meta // { commit_stamp = "bad"; })
    (meta // { commit_stamp = "20269999999999"; })
    (meta // { package_revision = 0; })
    (
      meta
      // {
        kind = "release";
        tag = "v0.2.0";
      }
    )
  ];
  local = mk {
    baseVersion = "0.1.0";
    sourceInfo.narHash = nar;
  };
  released = call (
    meta
    // {
      kind = "release";
      tag = "v0.1.0";
    }
  );
  first = call meta;
  later = call (
    meta
    // {
      sequence = 10;
      commit_epoch = 1;
      commit_stamp = "19700101000001";
    }
  );
  next = mk {
    baseVersion = "0.1.1";
    sourceInfo = info;
    metadata = meta // {
      kind = "release";
      tag = "v0.1.1";
      base_version = "0.1.1";
    };
  };
in
assert builtins.all rejected invalid;
assert !local.officialVersion && local.sourceRevision == null && local.sourceDateEpoch == 1;
assert released.debianVersion == "0.1.0-1" && released.archVersion == "0.1.0-1";
assert
  (mk {
    baseVersion = "0.1.0";
    sourceInfo.narHash = nar;
    metadata = meta;
  }).officialVersion;
pkgs.runCommand "nuschl-pure-distro-version-tests"
  {
    nativeBuildInputs = [
      pkgs.dpkg
      pkgs.pacman
    ];
  }
  ''
    dpkg --compare-versions ${pkgs.lib.escapeShellArg local.debianVersion} lt ${pkgs.lib.escapeShellArg released.debianVersion}
    test "$(vercmp ${pkgs.lib.escapeShellArg local.archVersion} ${pkgs.lib.escapeShellArg released.archVersion})" = -1
    dpkg --compare-versions ${pkgs.lib.escapeShellArg first.debianVersion} lt ${pkgs.lib.escapeShellArg later.debianVersion}
    test "$(vercmp ${pkgs.lib.escapeShellArg first.archVersion} ${pkgs.lib.escapeShellArg later.archVersion})" = -1
    dpkg --compare-versions ${pkgs.lib.escapeShellArg later.debianVersion} lt ${pkgs.lib.escapeShellArg next.debianVersion}
    test "$(vercmp ${pkgs.lib.escapeShellArg later.archVersion} ${pkgs.lib.escapeShellArg next.archVersion})" = -1
    touch "$out"
  ''
