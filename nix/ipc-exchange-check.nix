# SPDX-License-Identifier: EUPL-1.2
# Runs crates/ipc/tests/exchange.rs as the diagnostic executable it is
# (`harness = false`, its own main and its own fake worker). crate2nix test
# leaves compile every tests/*.rs under libtest, which drops that main and
# reports 0 tests, so the crash/garble/hang/linger exchange never ran in Nix.
#
# One rustc invocation compiles the unchanged fixture against the exact cached
# nuschl-ipc derivation and its complete dependency closure, with the toolchain
# that built them: no Cargo target and no other Rust graph. The diagnostic then
# runs under the real prlimit + bubblewrap worker sandbox. An unusable sandbox
# fails the check; it never skips.
#
# Linux only (the fixture's exchange is Linux-only); this is no evidence for the
# Windows sandbox, whose harness=false executable the Windows VM runs itself.
{
  pkgs,
  # The toolchain of the crate2nix graph (flake.nix `toolchain`).
  toolchain,
  # crates.project.workspaceMembers."nuschl-ipc".build
  ipc,
}:
let
  inherit (pkgs) lib coreutils runtimeShell;
  fixture = ../crates/ipc/tests/exchange.rs;
  workspace = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace;
  # The fixture prints this only after all of its assertions held.
  success = "exchange: a failing request costs only itself ... ok";
  # Requests in the fixture's batch, and the requests after which its crash,
  # garble and hang make the host start a fresh worker (1-based).
  batch = 8;
  restarts = [
    2
    4
    6
  ];
  # host::exchange waits this long for a worker to exit after its last reply.
  lingerSeconds = 2;

  bwrap = "${pkgs.bubblewrap}/bin/bwrap";
  prlimit = "${pkgs.util-linux}/bin/prlimit";

  # Same --extern spelling as buildRustCrate (mkRustcDepArgs).
  extern =
    dep:
    let
      name = lib.replaceStrings [ "-" ] [ "_" ] dep.libName;
      file =
        if lib.any (type: type == "lib" || type == "rlib") dep.crateType then
          "${dep.metadata}.rlib"
        else if dep.procMacro or false then
          "${dep.metadata}${pkgs.stdenv.buildPlatform.extensions.library}"
        else
          "${dep.metadata}${pkgs.stdenv.hostPlatform.extensions.library}";
    in
    "${name}=${lib.getLib dep}/lib/lib${name}-${file}";
  # Like a Cargo integration test: the package's library and its direct
  # dependencies are nameable; the complete closure resolves the rest.
  externs = map extern ([ ipc ] ++ ipc.dependencies);
  libraryDirs = map (dep: "${lib.getLib dep}/lib") (lib.unique ([ ipc ] ++ ipc.completeDeps));
  lintFlag = {
    forbid = "-F";
    deny = "-D";
    warn = "-W";
    allow = "-A";
  };
  lints = lib.mapAttrsToList (
    name: setting: "${lintFlag.${if builtins.isAttrs setting then setting.level else setting}} ${name}"
  ) (workspace.lints.rust or { });

  diagnostic =
    assert lib.assertMsg (
      !(ipc.buildTests or false)
    ) "pass the nuschl-ipc library build, not a test build";
    assert lib.assertMsg (lib.any (
      input: (input.outPath or null) == toolchain.outPath
    ) ipc.nativeBuildInputs) "nuschl-ipc was built with another toolchain; its rlibs would not load";
    pkgs.runCommandCC "nuschl-ipc-exchange-diagnostic"
      {
        nativeBuildInputs = [ toolchain ];
        meta.mainProgram = "exchange";
      }
      ''
        for library in ${lib.concatMapStringsSep " " (e: lib.last (lib.splitString "=" e)) externs}; do
          test -f "$library"
        done
        # Native link flags recorded by build scripts, as buildRustCrate links bins.
        links=()
        for dir in ${lib.concatStringsSep " " libraryDirs}; do
          if [ -s "$dir/link" ]; then
            while read -r flag; do links+=($flag); done < "$dir/link"
          fi
        done
        mkdir -p "$out/bin"
        rustc --edition ${workspace.package.edition} --crate-name exchange --crate-type bin \
          ${fixture} -o "$out/bin/exchange" \
          -C opt-level=0 -C debug-assertions=on -C overflow-checks=on \
          -C linker=${pkgs.stdenv.cc}/bin/${pkgs.stdenv.cc.targetPrefix}cc \
          ${lib.concatStringsSep " " lints} \
          ${lib.concatMapStringsSep " " (e: "--extern ${e}") externs} \
          ${lib.concatMapStringsSep " " (dir: "-L dependency=${dir}") libraryDirs} \
          "''${links[@]}"
      '';

  # Runs one executable as the diagnostic and verifies, from outside, that
  # each misbehaving worker was exercised. Evidence goes to $2.
  runner =
    { bwrap, prlimit }:
    pkgs.writeShellScript "nuschl-ipc-exchange-run" ''
      set -euo pipefail
      PATH=${
        lib.makeBinPath [
          coreutils
          pkgs.gnugrep
          pkgs.gnused
        ]
      }
      executable=$1
      evidence=$2
      fail() {
        printf 'ipc-exchange-check: FAIL: %s\n' "$*" >&2
        exit 1
      }
      mkdir -p "$evidence/shim"

      # The worker sandbox is part of what is checked: no skip without it.
      [ -x ${bwrap} ] || fail "bubblewrap is unavailable"
      [ -x ${prlimit} ] || fail "prlimit is unavailable"
      if ! ${bwrap} --die-with-parent --new-session --unshare-all \
          --ro-bind /nix/store /nix/store --proc /proc --dev /dev --tmpfs /tmp \
          --clearenv -- ${coreutils}/bin/true 2> "$evidence/preflight.stderr"; then
        cat "$evidence/preflight.stderr" >&2
        fail "bubblewrap cannot create the worker sandbox here"
      fi

      # Transparent recorders: each logs its arguments, then execs the real
      # helper with them unchanged (same PID, limits and namespaces). The
      # sandbox command clears the environment, so paths are absolute.
      launches=$evidence/launches.log
      : > "$launches"
      for helper in prlimit:${prlimit} bwrap:${bwrap}; do
        name=''${helper%%:*}
        printf '#!${runtimeShell}\nprintf "%%s %%s\\n" %s "$*" >> %s\nexec %s "$@"\n' \
          "$name" "$launches" "''${helper#*:}" > "$evidence/shim/$name"
        chmod 555 "$evidence/shim/$name"
      done

      # Builtins only between the snapshots: no process of this script's own.
      start=''${EPOCHREALTIME//[.,]/}
      before=(/proc/[0-9]*)
      set +e
      ${coreutils}/bin/timeout --kill-after=5 120 \
        ${coreutils}/bin/env -i PATH="$evidence/shim" "$executable" \
        > "$evidence/stdout" 2> "$evidence/stderr"
      status=$?
      set -e
      after=(/proc/[0-9]*)
      end=''${EPOCHREALTIME//[.,]/}
      elapsed=$(( (end - start) / 1000 ))
      # Processes started during the run that still exist as it returns.
      declare -A existed
      for process in "''${before[@]}"; do existed[$process]=1; done
      leftover=()
      for process in "''${after[@]}"; do
        [ -n "''${existed[$process]:-}" ] || leftover+=("''${process#/proc/}")
      done
      : > "$evidence/leftovers"
      for pid in "''${leftover[@]}"; do
        stat="(already gone)"
        read -r stat 2> /dev/null < "/proc/$pid/stat" || true
        printf '%s %s\n' "$pid" "$stat" >> "$evidence/leftovers"
      done
      printf '%s\n' "$status" > "$evidence/status"
      printf '%s\n' "$elapsed" > "$evidence/elapsed-ms"

      if [ "$status" -ne 0 ]; then
        cat "$evidence/stderr" >&2
        fail "diagnostic exited with status $status"
      fi
      [ "$(< "$evidence/stdout")" = ${lib.escapeShellArg success} ] ||
        fail "diagnostic did not print its success line"

      # Nothing it started outlives it, inside or outside its worker namespaces.
      # A worker namespace dies with its bubblewrap (die-with-parent, then
      # PID-namespace teardown) just after the host reaps that direct child,
      # so its processes may still be exiting at this instant. Anything still
      # alive (not a zombie) after 5 s outlived the diagnostic.
      alive=("''${leftover[@]}")
      waited=0
      while [ "''${#alive[@]}" -gt 0 ] && [ "$waited" -lt 5000 ]; do
        sleep 0.1
        waited=$(( waited + 100 ))
        still=()
        for pid in "''${alive[@]}"; do
          stat=""
          if read -r stat 2> /dev/null < "/proc/$pid/stat"; then
            state=''${stat##*) }
            [ "''${state%% *}" = Z ] || still+=("$pid")
          fi
        done
        alive=("''${still[@]}")
      done
      [ "''${#alive[@]}" -eq 0 ] ||
        fail "processes outlived the diagnostic by 5 s: $(cat "$evidence/leftovers")"

      # Every worker started in the sandbox, with its own CPU budget: the
      # batch's sum first, then the sum of what remained after the crash,
      # garble and hang requests. The final linger worker needs no restart.
      mapfile -t cpu < <(sed -n 's/^prlimit --as=[0-9]* --cpu=\([0-9]*\) -- .*$/\1/p' "$launches")
      [ "$(grep -c '^prlimit ' "$launches")" -eq ${toString (1 + builtins.length restarts)} ] &&
        [ "$(grep -c '^bwrap ' "$launches")" -eq ${toString (1 + builtins.length restarts)} ] &&
        [ "''${#cpu[@]}" -eq ${toString (1 + builtins.length restarts)} ] ||
        fail "expected ${
          toString (1 + builtins.length restarts)
        } sandboxed worker launches: $(cat "$launches")"
      budget=$(( cpu[0] / ${toString batch} ))
      expected="$(( budget * ${toString batch} ))${
        lib.concatMapStrings (request: " $(( budget * ${toString (batch - request)} ))") restarts
      }"
      [ "$budget" -gt 0 ] && [ "''${cpu[*]}" = "$expected" ] ||
        fail "workers did not restart after requests ${
          lib.concatMapStringsSep ", " toString restarts
        }: CPU budgets ''${cpu[*]}, expected $expected"
      while IFS= read -r line; do
        case $line in
          "bwrap "*) ;;
          *) continue ;;
        esac
        for option in --die-with-parent --new-session --unshare-all --clearenv; do
          [[ " $line " == *" $option "* ]] || fail "a worker started without $option: $line"
        done
        [[ $line == *" -- /worker" ]] || fail "unexpected worker command: $line"
      done < "$launches"

      # The hanging request waited out its whole budget and the lingering worker
      # its exit wait; the fixture itself bounds the total below 11 s.
      [ "$elapsed" -ge $(( (budget + ${toString lingerSeconds}) * 1000 )) ] ||
        fail "finished in $elapsed ms: the hang ($budget s) and linger (${toString lingerSeconds} s) waits did not both happen"

      {
        printf 'diagnostic: %s\n' "$(${coreutils}/bin/sha256sum "$executable")"
        printf 'stdout: %s\n' "$(< "$evidence/stdout")"
        printf 'sandboxed worker launches: %s, CPU budgets %s s\n' "''${#cpu[@]}" "''${cpu[*]}"
        printf 'elapsed: %s ms (per-request budget %s s, linger wait %s s)\n' "$elapsed" "$budget" ${toString lingerSeconds}
        printf 'still exiting as it returned: %s (%s), none alive after %s ms\n' \
          "''${#leftover[@]}" "$(tr '\n' ';' < "$evidence/leftovers")" "$waited"
      } | tee "$evidence/summary"
    '';

  realRunner = runner { inherit bwrap prlimit; };

  runWith =
    executable: passthru:
    pkgs.runCommand "nuschl-ipc-exchange-check" { inherit passthru; } ''
      ${realRunner} ${executable} "$out"
    '';

  failingBwrap = pkgs.writeShellScript "bwrap" "exit 1";
  failingBwrapPath = pkgs.runCommand "nuschl-failing-bwrap" { } ''
    install -Dm555 ${failingBwrap} "$out/bin/bwrap"
  '';

  # Each control passes only when its run fails for the stated reason.
  rejects =
    name: reason: command:
    pkgs.runCommand "nuschl-ipc-exchange-negative-${name}" { } ''
      mkdir -p "$out"
      set +e
      (
        ${command}
      ) > "$out/stdout" 2> "$out/stderr"
      status=$?
      set -e
      printf '%s\n' "$status" > "$out/status"
      cat "$out/stderr" >&2
      if [ "$status" -eq 0 ]; then
        echo "negative control ${name} passed; it must fail" >&2
        exit 1
      fi
      if grep -qxF ${lib.escapeShellArg success} "$out/stdout"; then
        echo "negative control ${name} printed the success line" >&2
        exit 1
      fi
      grep -qF -- ${lib.escapeShellArg reason} "$out/stderr" || {
        echo "negative control ${name} failed for another reason than: "${lib.escapeShellArg reason} >&2
        exit 1
      }
    '';
  # The diagnostic alone, with a chosen helper PATH.
  bare = path: ''
    ${coreutils}/bin/timeout --kill-after=5 120 \
      ${coreutils}/bin/env -i PATH=${path} ${diagnostic}/bin/exchange
  '';
in
runWith "${diagnostic}/bin/exchange" {
  inherit diagnostic;
  # Any executable in the diagnostic's place, e.g. to see the check refuse it.
  runWith = executable: runWith executable { };
  negativeControls = {
    # The checked binary replaced by `true`: exits 0 but proves nothing.
    replaced-by-true = rejects "replaced-by-true" "did not print its success line" ''
      ${realRunner} ${coreutils}/bin/true "$PWD/evidence"
    '';
    # The fixture itself fails closed without bubblewrap...
    diagnostic-without-bwrap = rejects "diagnostic-without-bwrap" ''MissingSandboxHelper("bwrap")'' (
      bare "${pkgs.util-linux}/bin"
    );
    # ...and when bubblewrap cannot start a worker: every request fails.
    diagnostic-failing-bwrap =
      rejects "diagnostic-failing-bwrap"
        ''left: ["exited", "exited", "exited", "exited", "exited", "exited", "exited", "exited"]''
        (bare "${failingBwrapPath}/bin:${pkgs.util-linux}/bin");
    # The check refuses before running when the sandbox is missing or broken.
    check-without-bwrap = rejects "check-without-bwrap" "bubblewrap is unavailable" ''
      ${
        runner {
          bwrap = "/nonexistent/bwrap";
          inherit prlimit;
        }
      } ${diagnostic}/bin/exchange "$PWD/evidence"
    '';
    check-failing-bwrap =
      rejects "check-failing-bwrap" "bubblewrap cannot create the worker sandbox here"
        ''
          ${
            runner {
              bwrap = "${failingBwrapPath}/bin/bwrap";
              inherit prlimit;
            }
          } ${diagnostic}/bin/exchange "$PWD/evidence"
        '';
  };
}
