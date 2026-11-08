{
  pkgs,
  nuschel,
  module,
}:
let
  postgresFixture = pkgs.writeText "nuschel-postgres-test-url" "postgresql:///nuschel?host=/run/postgresql&user=postgres\n";
in
pkgs.testers.runNixOSTest {
  name = "nuschel-mumble-client";
  nodes = {
    database =
      { lib, ... }:
      {
        imports = [ module ];
        environment.systemPackages = [
          nuschel
          pkgs.python3
        ];
        services.nuschel = {
          enable = true;
          package = nuschel;
          database.type = "postgresql";
          database.createLocally = true;
        };
        # Exercise the real hardened unit and database preflight. A protocol daemon
        # does not exist yet, so this smoke test deliberately runs configuration validation.
        systemd.services.nuschel.serviceConfig = {
          Type = lib.mkForce "oneshot";
          ExecStart = lib.mkForce "${nuschel}/bin/nuscheld --check-config";
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
          nuschel
          pkgs.python3
          pkgs.bubblewrap
          pkgs.util-linux
          pkgs.xvfb-run
          pkgs.xdotool
          pkgs.imagemagick
          pkgs.tesseract
          pkgs.pulseaudio
        ];
        hardware.graphics.enable = true;
        fonts.packages = [ pkgs.dejavu_fonts ];
        virtualisation.memorySize = 3072;
        users.users.tester = {
          isNormalUser = true;
          createHome = true;
        };
      };
  };
  testScript = ''
    start_all()
    server.wait_for_unit("murmur.service")
    server.wait_for_open_port(64738)
    server.succeed("systemctl stop murmur.service")
    server.succeed("${pkgs.python3}/bin/python3 ${../tests/mumble_fixture.py} /var/lib/murmur/murmur.sqlite")
    server.succeed("systemctl start murmur.service")
    server.wait_for_open_port(64738)
    client.wait_for_unit("multi-user.target")
    client.wait_until_succeeds("python3 -c 'import socket; socket.create_connection((\"server\", 64738), timeout=2).close()'")
    client.succeed("nuschel --help")
    client.succeed("nuscheld --help")
    # Run as an ordinary user so the sandbox test does not depend on root privileges.
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests/workers.py} ${nuschel}/bin --sandbox-server server'")
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/rtc_probe.py ${nuschel}/bin --opus-library ${pkgs.libopus}/lib/libopus.so'")
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests}/mumble_client.py ${nuschel}/bin --host server --opus-library ${pkgs.libopus}/lib/libopus.so'")
    client.succeed("su - tester -c 'xvfb-run -a -s \"-screen 0 1120x850x24\" ${pkgs.python3}/bin/python3 ${../tests}/desktop_mumble.py ${nuschel}/bin --host server --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${pkgs.libopus}/lib/libopus.so --output /tmp/nuschel-desktop-artifacts'")
    client.copy_from_machine("/tmp/nuschel-desktop-artifacts", "desktop")
    database.wait_for_unit("nuschel.service")
    database.succeed("su - postgres -c '${pkgs.python3}/bin/python3 ${../tests/workers.py} ${nuschel}/bin --postgres-url-file ${postgresFixture}'")
  '';
}
