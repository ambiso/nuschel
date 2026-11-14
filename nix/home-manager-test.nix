# Checks for the Home Manager module (nix/home-manager.nix).
#   files: evaluates Home Manager configurations and inspects what they generate.
#   vm:    a NixOS machine with Home Manager, where logging in autostarts Nuschl
#          and managed settings show as set by the system and stay so.
{
  pkgs,
  home-manager,
  module,
  nuschl,
}:
let
  inherit (pkgs) lib;
  homeWith =
    extra:
    home-manager.lib.homeManagerConfiguration {
      inherit pkgs;
      modules = [
        module
        {
          home = {
            username = "alice";
            homeDirectory = "/home/alice";
            stateVersion = "26.05";
          };
          programs.nuschl.enable = true;
        }
        extra
      ];
    };
  pin = builtins.hashString "sha256" "fixture certificate";
  full = homeWith {
    programs.nuschl = {
      autostart = true;
      initialSettings = {
        enter_sends = false;
        push_to_talk_key = "f13";
        audio.transmit = "push_to_talk";
        voice_bitrate = 96000;
      };
      initialServers = [
        {
          name = "Team";
          host = "Voice.Example.org";
          username = "alice";
          tokens = [ "blue" ];
        }
        {
          host = "::1";
          port = 64739;
          username = "alice";
          autoConnect = false;
          certificateSha256 = lib.toUpper pin;
        }
      ];
      settings = {
        enter_sends = false;
        audio.transmit = "push_to_talk";
        voice_bitrate = 192000;
      };
      servers = [
        # Takes over the initial bookmark for the same endpoint and user.
        {
          name = "Team";
          host = "voice.example.org";
          username = "alice";
          tokens = [ "blue" ];
        }
        {
          host = "Managed.Example.org";
          port = 1234;
          username = "bob";
          autoConnect = false;
          certificateSha256 = lib.toUpper pin;
        }
      ];
    };
  };
  readOnly = homeWith {
    xdg.autostart = {
      enable = true;
      readOnly = true;
    };
    programs.nuschl.autostart = true;
  };
  plain = homeWith { };
  decorated = homeWith {
    programs.nuschl = {
      autostart = true;
      windowDecorations = "server";
    };
  };
  legacy = homeWith { programs.nuschel.settings.force_tcp = true; };
  # Key-transparency witnesses as typed options (docs/witness.md).
  witnessKey = lib.removeSuffix "\n" (builtins.readFile ../tests/fixtures/witness-test.vkey);
  witnessed = homeWith {
    programs.nuschl.keyTransparency = {
      witnesses = [ { key = witnessKey; } ];
      maxAgeHours = 12;
    };
  };

  # Bookmarks and settings crates/session/src/config.rs would refuse (Nuschl
  # would set the file aside or ignore it) fail evaluation instead.
  refusedServers =
    option:
    let
      server = attrs: {
        programs.nuschl.${option} = [
          (
            {
              host = "voice.example.org";
              username = "alice";
            }
            // attrs
          )
        ];
      };
    in
    lib.mapAttrs' (name: lib.nameValuePair "${option}-${name}") {
      host-with-space = server { host = "voice example.org"; };
      host-with-slash = server { host = "voice.example.org/x"; };
      bracketed-ipv6 = server { host = "[::1]"; };
      port-zero = server { port = 0; };
      blank-username = server { username = "  "; };
      long-username = server { username = lib.concatStrings (lib.genList (_: "a") 129); };
      short-pin = server { certificateSha256 = "abcd"; };
      long-token = server { tokens = [ (lib.concatStrings (lib.genList (_: "a") 257)) ]; };
      duplicate.programs.nuschl.${option} = [
        {
          host = "voice.example.org";
          username = "alice";
        }
        {
          name = "Again";
          host = "VOICE.example.org";
          username = "alice";
        }
      ];
    };
  refused =
    refusedServers "initialServers"
    // refusedServers "servers"
    // {
      unknown-setting.programs.nuschl.settings.enter_send = false;
      witness-without-key-id.programs.nuschl.keyTransparency.witnesses = [ { key = "witness+AQ=="; } ];
      witness-max-age.programs.nuschl.keyTransparency.maxAgeHours = 169;
    };
  # A type error throws; a failed assertion is false. Forcing only these
  # is much cheaper than evaluating the whole activation package.
  accepts =
    extra:
    let
      inherit (homeWith extra) config;
      inherit (config.programs.nuschl) initialServers servers keyTransparency;
      result = builtins.tryEval (
        builtins.deepSeq [ initialServers servers keyTransparency ] (
          lib.all (a: a.assertion) config.assertions
        )
      );
    in
    result.success && result.value;
  accepted = lib.attrNames (lib.filterAttrs (_: accepts) refused);

  # Every field of ClientSettings is a setting the module accepts.
  settingFields =
    let
      source = builtins.readFile ../crates/session/src/config.rs;
      body = lib.head (
        lib.splitString "\n}" (lib.elemAt (lib.splitString "pub struct ClientSettings {" source) 1)
      );
      field = line: builtins.match " *(pub )?([a-z_][a-z0-9_]*): .*" line;
    in
    map (match: lib.elemAt match 1) (
      lib.filter (match: match != null) (map field (lib.splitString "\n" body))
    );

  autostartSource = ../apps/desktop/src/autostart.rs;
in
assert lib.assertMsg legacy.config.programs.nuschl.settings.force_tcp
  "legacy programs.nuschel alias lost settings";
assert lib.assertMsg (
  full.config.programs.nuschl.package.outPath == nuschl.outPath
) "programs.nuschl.package does not default to this flake's package";
assert lib.assertMsg (accepted == [ ]) "invalid bookmarks were accepted: ${toString accepted}";
assert lib.assertMsg (lib.length settingFields > 15) "could not read ClientSettings' fields";
assert lib.assertMsg (accepts {
  programs.nuschl.settings = lib.genAttrs settingFields (_: null);
}) "programs.nuschl.settings refuses a field of ClientSettings: update settingNames";
{
  files =
    pkgs.runCommand "nuschl-home-manager-files"
      {
        nativeBuildInputs = [
          pkgs.desktop-file-utils
          pkgs.jq
        ];
      }
      ''
        home=${full.activationPackage}

        # The autostart entry is the one the in-app switch writes, with this
        # package's binary in place of the program the app finds.
        entry=$home/home-files/.config/autostart/org.nuschl.Nuschl.desktop
        desktop-file-validate "$entry"
        template=$(grep -o '"\[Desktop Entry\]\\n[^"]*"' ${autostartSource}) || {
          echo "apps/desktop/src/autostart.rs no longer has the entry's format string" >&2
          exit 1
        }
        flag=$(sed -n 's/^pub const FLAG: &str = "\(.*\)";$/\1/p' ${autostartSource})
        test "$flag" = --autostart
        template=''${template:1:-1}
        template=''${template//\{program\}/${lib.getExe nuschl}}
        template=''${template//\{FLAG\}/$flag}
        printf '%b' "$template" > expected.desktop
        if ! cmp expected.desktop "$entry"; then
          diff -u expected.desktop "$entry" >&2 || true
          echo "Update the autostart entry in nix/home-manager.nix to match the app's" >&2
          exit 1
        fi

        # The package, its desktop entry and icons.
        test "$(readlink -f "$home/home-path/bin/nuschl")" = "$(readlink -f ${nuschl}/bin/nuschl)"
        test -e "$home/home-path/share/applications/org.nuschl.Nuschl.desktop"
        test -e "$home/home-path/share/icons/hicolor/scalable/apps/org.nuschl.Nuschl.svg"

        # A read-only autostart folder lists the same entry.
        test -L ${readOnly.activationPackage}/home-files/.config/autostart
        cmp "$entry" ${readOnly.activationPackage}/home-files/.config/autostart/org.nuschl.Nuschl.desktop

        # Managed settings and bookmarks: a read-only file in Nuschl's folder,
        # with the ids the same endpoints get as initial bookmarks.
        managed=$home/home-files/.config/nuschl/managed.json
        test -L "$managed"
        jq -e '.settings == {enter_sends: false, audio: {transmit: "push_to_talk"}}' "$managed"
        # The voice bitrate stays out of managed.json, which an earlier
        # Nuschl would otherwise ignore as a whole (docs: config.rs).
        test -L $home/home-files/.config/nuschl/managed-voice-quality.json
        jq -e '. == {version: 1, voice_bitrate: 192000}' $home/home-files/.config/nuschl/managed-voice-quality.json
        jq -e '.servers | length == 2 and .[0].id != .[1].id' "$managed"
        jq -e '.servers[1] == {id: .servers[1].id, name: "Managed.Example.org", host: "managed.example.org",
          port: 1234, username: "bob", tokens: [], certificate_sha256: "${pin}", auto_connect: false}' "$managed"

        # Witnesses as typed options land in managed.json as key_transparency.
        jq -e --arg key "${witnessKey}" '.settings.key_transparency == {default_witnesses: true,
          witnesses: [{key: $key, enabled: true}], max_age_hours: 12, block_new_devices: true}' \
          ${witnessed.activationPackage}/home-files/.config/nuschl/managed.json

        # The window decorations preference: the session's environment, the
        # user manager's, and the autostart entry's arguments.
        decorated=${decorated.activationPackage}
        grep -qFx 'export NUSCHL_WINDOW_DECORATIONS="server"' $decorated/home-path/etc/profile.d/hm-session-vars.sh
        grep -qFx 'NUSCHL_WINDOW_DECORATIONS=server' $decorated/home-files/.config/environment.d/10-home-manager.conf
        desktop-file-validate $decorated/home-files/.config/autostart/org.nuschl.Nuschl.desktop
        grep -qFx 'Exec=${lib.getExe nuschl} --autostart --window-decorations server' \
          $decorated/home-files/.config/autostart/org.nuschl.Nuschl.desktop
        if grep -q NUSCHL_WINDOW_DECORATIONS ${plain.activationPackage}/home-path/etc/profile.d/hm-session-vars.sh; then
          echo "the decorations preference is set although none was chosen" >&2
          exit 1
        fi

        # Without autostart or initial files the module installs the package only.
        test ! -e ${plain.activationPackage}/home-files/.config/nuschl/managed.json
        test ! -e ${plain.activationPackage}/home-files/.config/autostart
        if grep -q nuschl-initial-files ${plain.activationPackage}/activate; then
          echo "initial files are written although none are set" >&2
          exit 1
        fi
        test -e ${plain.activationPackage}/home-path/bin/nuschl

        # The activation step and what it writes.
        seed=$(grep -o '/nix/store/[a-z0-9]*-nuschl-initial-files' "$home/activate" | head -n 1)
        grep -qF "run $seed /home/alice/.config/nuschl" "$home/activate"
        dir=$PWD/home/alice/.config/nuschl
        "$seed" "$dir" > first.log
        grep -qFx "Nuschl: wrote $dir/settings.json" first.log
        grep -qFx "Nuschl: wrote $dir/servers.json" first.log
        grep -qFx "Nuschl: wrote $dir/voice-quality.json" first.log
        test "$(stat -c %a "$dir/voice-quality.json")" = 600
        jq -e '. == {version: 1, voice_bitrate: 96000}' "$dir/voice-quality.json"
        test "$(stat -c %a "$dir")" = 700
        test "$(stat -c %a "$dir/settings.json")" = 600
        test "$(stat -c %a "$dir/servers.json")" = 600
        jq -e '. == {enter_sends: false, push_to_talk_key: "f13", audio: {transmit: "push_to_talk"}}' "$dir/settings.json"
        uuid='^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-a[0-9a-f]{3}-[0-9a-f]{12}$'
        jq -e --arg uuid "$uuid" 'length == 2 and all(.[]; .id | test($uuid)) and .[0].id != .[1].id' "$dir/servers.json"
        jq -e '.[0] == {id: .[0].id, name: "Team", host: "voice.example.org", port: 64738, username: "alice",
          tokens: ["blue"], certificate_sha256: null, auto_connect: true}' "$dir/servers.json"
        jq -e '.[1] == {id: .[1].id, name: "::1", host: "::1", port: 64739, username: "alice",
          tokens: [], certificate_sha256: "${pin}", auto_connect: false}' "$dir/servers.json"
        jq -e --slurpfile managed "$managed" '.[0] == $managed[0].servers[0]' "$dir/servers.json"

        # Once a file exists Nuschl owns it; a deleted one is written again.
        echo '[]' > "$dir/servers.json"
        rm "$dir/settings.json"
        "$seed" "$dir" > second.log
        test "$(cat "$dir/servers.json")" = "[]"
        test "$(cat second.log)" = "Nuschl: wrote $dir/settings.json"
        test -z "$(find "$dir" -name '.*.home-manager')"
        touch "$out"
      '';

  vm = pkgs.testers.runNixOSTest {
    name = "nuschl-home-manager";
    nodes.machine = {
      imports = [ home-manager.nixosModules.home-manager ];
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };
      home-manager = {
        useGlobalPkgs = true;
        useUserPackages = true;
        users.alice = {
          imports = [ module ];
          home.stateVersion = "26.05";
          programs.nuschl = {
            enable = true;
            autostart = true;
            # The user's own choice, under a managed one.
            initialSettings.enter_sends = true;
            initialServers = [
              {
                name = "Local";
                host = "127.0.0.1";
                username = "alice";
              }
            ];
            settings = {
              enter_sends = false;
              shortcuts."push-to-talk" = "f13";
            };
            servers = [
              {
                name = "Managed";
                host = "localhost";
                username = "bob";
              }
            ];
          };
        };
      };
      # The bookmark's server: its log shows when the autostarted app connects.
      services.murmur.enable = true;
      # What a desktop session provides: a display, and a target that starts
      # the units systemd generates from XDG autostart entries.
      systemd.user.services.xvfb.serviceConfig.ExecStart =
        "${pkgs.xvfb}/bin/Xvfb :99 -screen 0 1280x800x24";
      systemd.user.targets.test-session.wants = [ "xdg-desktop-autostart.target" ];
      hardware.graphics.enable = true;
      fonts.packages = [ pkgs.dejavu_fonts ];
      environment.systemPackages = [
        pkgs.xdotool
        pkgs.xvfb-run
      ];
      virtualisation.memorySize = 2048;
      virtualisation.cores = 2;
    };
    testScript = ''
      import sys
      sys.path.insert(0, "${../tests}")
      import quarantine  # type: ignore  # tests/quarantine.py, on the path above

      user = "--user -M alice@"
      unit = "app-org.nuschl.Nuschl@autostart.service"
      entry = "/home/alice/.config/autostart/org.nuschl.Nuschl.desktop"
      config = "/home/alice/.config/nuschl"
      data = "/home/alice/.local/share/nuschl"

      machine.wait_for_unit("home-manager-alice.service")
      machine.wait_for_unit("murmur.service")

      with subtest("the package is on the user's PATH with its desktop entry and icons"):
          machine.succeed("su - alice -c 'nuschl --version' | grep -Fx 'nuschl 0.1.0'")
          machine.succeed("test \"$(su - alice -c 'readlink -f \"$(command -v nuschl)\"')\" = ${nuschl}/bin/nuschl")
          machine.succeed("test -e /etc/profiles/per-user/alice/share/applications/org.nuschl.Nuschl.desktop")
          machine.succeed("test -e /etc/profiles/per-user/alice/share/icons/hicolor/scalable/apps/org.nuschl.Nuschl.svg")

      with subtest("the initial files are private to alice"):
          machine.succeed(f"test \"$(stat -c '%a %U' {config})\" = '700 alice'")
          machine.succeed(f"test \"$(stat -c '%a %U' {config}/servers.json {config}/settings.json)\" = \"$(printf '600 alice\\n600 alice')\"")
          machine.succeed(f"grep -F '\"host\": \"127.0.0.1\"' {config}/servers.json")

      with subtest("logging in starts Nuschl minimised from the autostart entry"):
          machine.succeed("loginctl enable-linger alice")
          machine.wait_for_unit("user@1000.service")
          machine.wait_for_unit("default.target", "alice")
          # systemd's XDG autostart generator turned the entry into a unit.
          machine.succeed(f"systemctl {user} show -p ExecStart {unit} | grep -F 'argv[]=${nuschl}/bin/nuschl --autostart ;'")
          machine.succeed(f"systemctl {user} start xvfb.service")
          machine.wait_until_succeeds("test -S /tmp/.X11-unix/X99")
          machine.succeed(f"systemctl {user} set-environment DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1 LP_NUM_THREADS=2")
          machine.succeed(f"systemctl {user} start test-session.target")
          machine.wait_until_succeeds(f"systemctl {user} is-active {unit}")
          machine.wait_until_succeeds("su - alice -c 'DISPLAY=:99 xdotool search --name ^Nuschl$'")
          # It read the seeded bookmark and connected on start.
          machine.wait_until_succeeds("journalctl -u murmur.service --grep 'New connection'")
          machine.succeed(f"systemctl {user} is-active {unit}")
          machine.succeed(f"systemctl {user} stop {unit}")

      with subtest("the identity and history are in the data folder, not with the dotfiles"):
          with quarantine.step("vm-home-manager/config-folder-listing"):
              machine.succeed(f"test \"$(ls {config})\" = \"$(printf 'managed.json\\nnuschl-namespace-v1\\nservers.json\\nsettings.json')\"")
          machine.succeed(f"test -s {data}/identity/identity.json")
          machine.succeed(f"test \"$(stat -c '%a %U' {data} {data}/identity {data}/history)\" = \"$(printf '700 alice\\n700 alice\\n700 alice')\"")
          machine.succeed(f"test \"$(stat -c %a {data}/identity/identity.json)\" = 600")

      with subtest("managed settings and bookmarks show as set by the system and stay so"):
          machine.succeed(f"test -L {config}/managed.json")
          # Over AT-SPI, as a screen reader and a pointer meet them (see the script).
          machine.succeed(f"su - alice -c '${pkgs.dbus}/bin/dbus-run-session -- xvfb-run -a -s \"-screen 0 1280x800x24\" ${pkgs.python3}/bin/python3 ${../tests}/managed_config.py ${nuschl}/bin/nuschl --at-spi ${pkgs.at-spi2-core} --config {config}'")
          machine.succeed(f"grep -F '\"enter_sends\": true' {config}/settings.json")
          machine.succeed(f"test -L {config}/managed.json")

      with subtest("switching again keeps Nuschl's files and replaces an entry the app wrote"):
          # The in-app switch turned off and on again: the app's own file in
          # place of the link (it names the program on PATH).
          machine.succeed(f"su - alice -c 'sed \"s|^Exec=.*|Exec=nuschl --autostart|\" {entry} > {entry}.new && mv {entry}.new {entry}'")
          machine.succeed(f"test ! -L {entry}")
          machine.succeed(f"su - alice -c 'echo [] > {config}/servers.json'")
          machine.succeed("systemctl restart home-manager-alice.service")
          machine.succeed(f"test -L {entry}")
          machine.succeed(f"test \"$(cat {config}/servers.json)\" = '[]'")
    '';
  };
}
