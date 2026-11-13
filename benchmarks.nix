# SPDX-License-Identifier: EUPL-1.2
# Performance numbers, run locally: `nix build -f benchmarks.nix` (outside ci.nix;
# docs/development.md, "Benchmarks"). `--arg runs 10` repeats the one-core tests.
{
  runs ? 5,
}:
let
  flake = builtins.getFlake ("git+file://" + toString ./.);
  system = builtins.currentSystem;
  pkgs = flake.inputs.nixpkgs.legacyPackages.${system};
  inherit (pkgs) lib;
  nuschl = flake.packages.${system}.default;
  rtcProbe = flake.packages.${system}.rtc-probe;
  python = "${pkgs.python3}/bin/python3";
  opus = "--opus-library ${pkgs.libopus}/lib/libopus.so";

  # The tests of an existing VM check, `runs` times each on its node's only core
  # beside a busy loop (bench/one_core.py).
  oneCore =
    {
      name,
      check,
      node,
      # Where the tested processes run; the busy loop joins them there.
      slice,
      setup ? "",
      workers,
      tests,
    }:
    check.extend {
      modules = [
        {
          name = lib.mkForce name;
          # The driver imports bench/one_core.py.
          skipTypeCheck = true;
          nodes.${node}.virtualisation.cores = lib.mkForce 1;
          testScript = lib.mkForce ''
            import json
            import sys
            sys.path.insert(0, "${./bench}")
            import one_core
            ${node}.start()
            ${node}.wait_for_unit("multi-user.target")
            ${setup}
            one_core.run(${node}, ${builtins.toJSON tests}, ${toString runs}, "${slice}",
                         "${python} ${./bench/worker_cpu.py} ${lib.concatStringsSep " " workers}",
                         driver.out_dir, json.loads(open("${./bench/baseline.json}").read()).get("one_core", {}))
          '';
        }
      ];
    };
  mediaLive =
    part:
    "su - tester -c 'NUSCHL_MEDIA_ONLY=${part} NUSCHL_AUDIO_TIMING=1 ${python} ${./tests}/media_live.py ${nuschl}/bin --pulse-bin ${pkgs.pulseaudio}/bin ${opus} --delay-plugin ${pkgs.ladspa-sdk}/lib/ladspa/delay.so'";
in
{
  # Instructions per second of audio of the capture chain and Opus encoder, and
  # the echo reduction, on a fixed synthetic minute; fails above the baseline.
  # Built on this machine: the baseline holds for one CPU model.
  instructions =
    pkgs.runCommand "nuschl-bench-instructions"
      {
        nativeBuildInputs = [ pkgs.perf ];
        preferLocalBuild = true;
        allowSubstitutes = false;
      }
      "${python} ${./bench/instructions.py} ${nuschl}/bin/nuschl-media-worker ${./bench/baseline.json} $out";

  one-core = oneCore {
    name = "nuschl-one-core";
    check = flake.checks.${system}.vm-smoke;
    node = "client";
    # su - tester
    slice = "user.slice";
    workers = [ "nuschl-media-worker" ];
    tests = {
      media-jitter = mediaLive "jitter";
      media-echo = mediaLive "echo_test";
      media-double-talk = mediaLive "double_talk";
      # Transmits continuously: capture-to-send latency.
      media-sounds = mediaLive "sounds";
      rtc-probe = "su - tester -c '${python} ${./tests}/rtc_probe.py ${nuschl}/bin --probe ${rtcProbe}/bin/nuschl-rtc-probe ${opus}'";
      desktop-recovery = "su - tester -c 'xvfb-run -a -s \"-screen 0 1120x850x24\" ${python} ${./tests}/desktop_recovery.py ${nuschl}/bin --murmur ${pkgs.murmur}/bin/mumble-server --pulse-bin ${pkgs.pulseaudio}/bin ${opus} --at-spi ${pkgs.at-spi2-core} --dbus-daemon ${pkgs.dbus}/bin/dbus-daemon --output $(mktemp -d)'";
    };
  };

  one-core-musicbot = oneCore {
    name = "nuschl-one-core-musicbot";
    check = flake.checks.${system}.vm-musicbot;
    node = "machine";
    # The bot's service and the test script (the driver's backdoor service).
    slice = "system.slice";
    setup = ''
      machine.wait_for_open_port(64738)
      machine.wait_for_open_port(4533)
      machine.wait_until_succeeds("journalctl -u nuschl-musicbot | grep -q 'root: connected'")
    '';
    workers = [
      "nuschl-musicbot"
      "nuschl-musicbot-codec-worker"
    ];
    tests.musicbot-playback = "${python} ${./tests}/musicbot.py /run/current-system/sw/bin 64738 --service";
  };
}
