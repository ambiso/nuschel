{
  pkgs,
  nuschel,
  module,
}:
let
  postgresFixture = pkgs.writeText "nuschel-postgres-test-url" "postgresql:///nuschel?host=/run/postgresql&user=postgres\n";
in
pkgs.testers.runNixOSTest {
  name = "nuschel-foundation";
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
        ];
        virtualisation.memorySize = 2048;
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
    client.wait_for_unit("multi-user.target")
    client.wait_until_succeeds("python3 -c 'import socket; socket.create_connection((\"server\", 64738), timeout=2).close()'")
    client.succeed("nuschel --help")
    client.succeed("nuscheld --help")
    # Run as an ordinary user so the sandbox test does not depend on root privileges.
    client.succeed("su - tester -c '${pkgs.python3}/bin/python3 ${../tests/workers.py} ${nuschel}/bin --sandbox-server server'")
    database.wait_for_unit("nuschel.service")
    database.succeed("su - postgres -c '${pkgs.python3}/bin/python3 ${../tests/workers.py} ${nuschel}/bin --postgres-url-file ${postgresFixture}'")
  '';
}
