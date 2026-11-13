{
  pkgs,
  nuschl,
  rtcProbe,
  module,
  legacyServers,
}:
let
  postgresFixture = pkgs.writeText "nuschl-postgres-test-url" "postgresql:///nuschl?host=/run/postgresql&user=postgres\n";
in
pkgs.testers.runNixOSTest {
  name = "nuschl-mumble-client";
  nodes = {
    database =
      { lib, ... }:
      {
        imports = [ module ];
        environment.systemPackages = [
          nuschl
          pkgs.python3
        ];
        services.nuschl = {
          enable = true;
          package = nuschl;
          database.type = "postgresql";
          database.createLocally = true;
        };
        # Exercise the hardened unit and PostgreSQL preflight here; the protocol
        # daemon itself is tested with stock clients in vm-server.nix.
        systemd.services.nuschl.serviceConfig = {
          Type = lib.mkForce "oneshot";
          ExecStart = lib.mkForce "${nuschl}/bin/nuschld --check-config";
          RemainAfterExit = true;
          Restart = lib.mkForce "no";
        };
        virtualisation.memorySize = 1536;
      };
    server =
      { ... }:
      {
        services.murmur = {
          enable = true;
          openFirewall = true;
          # Negative certificate probes and independent test profiles share one VM IP.
          autobanAttempts = 0;
        };
      };
    client =
      { ... }:
      {
        environment.systemPackages = [
          nuschl
          pkgs.python3
          pkgs.bubblewrap
          pkgs.util-linux
          pkgs.xvfb-run
          pkgs.xdotool
          # stock_client.py reads the stock window's chat from the clipboard;
          # xwd keeps the screen of a failed run.
          pkgs.xclip
          pkgs.xwd
          pkgs.imagemagick
          pkgs.tesseract
          pkgs.pulseaudio
          pkgs.mumble
          pkgs.openssl
          pkgs.iptables
          pkgs.iproute2
          pkgs.procps
        ];
        hardware.graphics.enable = true;
        fonts.packages = [ pkgs.dejavu_fonts ];
        virtualisation.memorySize = 4096;
        virtualisation.cores = 2;
        users.users.tester = {
          isNormalUser = true;
          createHome = true;
        };
      };
  };
  testScript = ''
    start_all()
    # CI sample 2 for #10.
    server.wait_for_unit("murmur.service")
    server.wait_for_open_port(64738)
    server.succeed("systemctl stop murmur.service")
    server.succeed("${pkgs.python3}/bin/python3 ${../tests/mumble_fixture.py} /var/lib/murmur/murmur.sqlite")
    server.succeed("systemctl start murmur.service")
    server.wait_for_open_port(64738)
    # A SuperUser password for the administration tests.
    # The copy must be readable by the murmur user; -supw exits 0 once the password is set.
    server.succeed("install -m 0644 /run/murmur/murmurd.ini /tmp/murmurd.ini && systemctl stop murmur.service")
    server.succeed("runuser -u murmur -- ${pkgs.murmur}/bin/mumble-server -ini /tmp/murmurd.ini -supw nuschl-super")
    server.succeed("systemctl start murmur.service")
    server.wait_for_open_port(64738)
    client.wait_for_unit("multi-user.target")
    client.wait_until_succeeds("python3 -c 'import socket; socket.create_connection((\"server\", 64738), timeout=2).close()'")
    client.succeed("nuschl --help")
    client.succeed("nuschld --help")
    # Run as an ordinary user so the sandbox test does not depend on root privileges.
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests/workers.py} ${nuschl}/bin --sandbox-server server'")
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/rtc_probe.py ${nuschl}/bin --probe ${rtcProbe}/bin/nuschl-rtc-probe --opus-library ${pkgs.libopus}/lib/libopus.so'")
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/mumble_client.py ${nuschl}/bin --host server --opus-library ${pkgs.libopus}/lib/libopus.so'")
    # The desktop suites find controls over AT-SPI, on a private session bus of their own.
    # Screenshots, trees and logs are copied out on failure too (nix build --keep-failed).
    try:
        client.succeed("su - tester -c 'xvfb-run -a -s \"-screen 0 1120x850x24\" ${pkgs.python3}/bin/python3 ${../tests}/desktop_mumble.py ${nuschl}/bin --host server --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${pkgs.libopus}/lib/libopus.so --at-spi ${pkgs.at-spi2-core} --dbus-daemon ${pkgs.dbus}/bin/dbus-daemon --output /tmp/nuschl-desktop-artifacts'")
    finally:
        client.copy_from_machine("/tmp/nuschl-desktop-artifacts", "desktop")
    # Encrypted UDP with forced TCP, a lossy network and blocked inbound UDP (root for tc/iptables).
    client.succeed("${pkgs.python3}/bin/python3 ${../tests}/udp_voice.py ${nuschl}/bin --host server --opus-library ${pkgs.libopus}/lib/libopus.so --netem --block-udp")
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/protocol_features.py ${nuschl}/bin --host server --opus-library ${pkgs.libopus}/lib/libopus.so --openssl ${pkgs.openssl.bin}/bin/openssl'")
    # The echo test's room delays the speakers with the LADSPA SDK's example delay line.
    # print() puts every attempt's lines in the test log; the driver logs the output of failed commands.
    print(client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/media_live.py ${nuschl}/bin --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${pkgs.libopus}/lib/libopus.so --delay-plugin ${pkgs.ladspa-sdk}/lib/ladspa/delay.so'"))
    # Private PipeWire session (virtual nodes only): restarts, host pinning, echo-cancel reuse.
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/pipewire_recovery.py ${nuschl}/bin --pipewire ${pkgs.pipewire} --wireplumber ${pkgs.wireplumber} --dbus-daemon ${pkgs.dbus}/bin/dbus-daemon --opus-library ${pkgs.libopus}/lib/libopus.so --output /tmp/nuschl-pipewire-artifacts'")
    client.copy_from_machine("/tmp/nuschl-pipewire-artifacts", "pipewire")
    # Pure-Rust Opus against reference libopus: decoder conformance, interop, hostile packets.
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/opus_eval.py ${nuschl}/bin --opus-library ${pkgs.libopus}/lib/libopus.so'")
    client.succeed("su - tester -c 'xvfb-run -a -s \"-screen 0 1280x800x24\" ${pkgs.python3}/bin/python3 ${../tests}/stock_client.py ${nuschl}/bin --mumble ${pkgs.mumble}/bin/mumble --host server --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${pkgs.libopus}/lib/libopus.so --output /tmp/nuschl-stock-artifacts'")
    try:
        client.succeed("su - tester -c 'xvfb-run -a -s \"-screen 0 1120x850x24\" ${pkgs.python3}/bin/python3 ${../tests}/desktop_recovery.py ${nuschl}/bin --murmur ${pkgs.murmur}/bin/mumble-server --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${pkgs.libopus}/lib/libopus.so --at-spi ${pkgs.at-spi2-core} --dbus-daemon ${pkgs.dbus}/bin/dbus-daemon --output /tmp/nuschl-recovery-artifacts'")
    finally:
        client.copy_from_machine("/tmp/nuschl-recovery-artifacts", "recovery")
    # Screen-reader view over AT-SPI: names and roles, and operating Mute without a pointer.
    try:
        client.succeed("su - tester -c '${pkgs.dbus}/bin/dbus-run-session -- xvfb-run -a -s \"-screen 0 1280x800x24\" ${pkgs.python3}/bin/python3 ${../tests}/accessibility.py ${nuschl}/bin --murmur ${pkgs.murmur}/bin/mumble-server --at-spi ${pkgs.at-spi2-core} --output /tmp/nuschl-a11y-artifacts'")
    finally:
        client.copy_from_machine("/tmp/nuschl-a11y-artifacts", "accessibility")
    # Older stock servers use the legacy UDP packet format.
    for version, server_binary, extra in [("1.4", "${legacyServers."1.4"}", ""), ("1.3", "${legacyServers."1.3"}", " --no-listening")]:
        client.succeed(f"su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/with_local_murmur.py {server_binary} ${../tests}/udp_voice.py ${nuschl}/bin --port PORT --opus-library ${pkgs.libopus}/lib/libopus.so'")
        client.succeed(f"su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/with_local_murmur.py {server_binary} ${../tests}/protocol_features.py ${nuschl}/bin --port PORT --opus-library ${pkgs.libopus}/lib/libopus.so --openssl ${pkgs.openssl.bin}/bin/openssl{extra}'")
        print(f"Stock Mumble {version}: UDP voice and protocol features passed.")
    database.wait_for_unit("nuschl.service")
    database.succeed("su - postgres -c '${pkgs.python3}/bin/python3 ${../tests/workers.py} ${nuschl}/bin --postgres-url-file ${postgresFixture}'")
  '';
}
