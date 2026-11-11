# nuscheld under the NixOS module, with stock clients: the stock Mumble GUI
# (login with a Nuschel-exported certificate, voice both ways) and the same
# client suites vm-smoke runs against stock Murmur; and Nuschel with the
# extension's server history (SQLite and PostgreSQL) and key transparency (its
# keys as credentials on one node, generated on the other). A second node runs
# the server on local PostgreSQL. State must survive service restarts.
{
  pkgs,
  nuschel,
  module,
}:
let
  superuser = "nuschel-super";
  opus = "${pkgs.libopus}/lib/libopus.so";
  # The SuperUser password is a runtime file (the module refuses store paths);
  # the test writes it before (re)starting the service.
  serverNode =
    extra:
    { lib, ... }:
    {
      imports = [ module ];
      services.nuschel = {
        enable = true;
        package = nuschel;
        openFirewall = true;
        superUserPasswordFile = "/run/keys/nuschel-superuser";
        settings = {
          welcome_text = "Welcome to the <b>Nuschel</b> VM server.";
          max_bandwidth = 72000;
          # Negative certificate probes and several test profiles share one client IP.
          autoban.attempts = 0;
          password_hashing = {
            memory_kib = 4096;
            passes = 1;
          };
        };
      }
      // extra;
      # Started by the test once the password file exists.
      systemd.services.nuschel.wantedBy = lib.mkForce [ ];
      virtualisation.memorySize = 1536;
    };
in
pkgs.testers.runNixOSTest {
  name = "nuschel-server";
  nodes = {
    server = serverNode {
      # Key-transparency keys as credentials (written by the test, never in the store).
      kt.signingKeyFile = "/run/keys/nuschel-kt-signing";
      kt.vrfSecretFile = "/run/keys/nuschel-kt-vrf";
    };
    postgres = serverNode {
      database.type = "postgresql";
      database.createLocally = true;
    };
    client =
      { ... }:
      {
        environment.systemPackages = [
          nuschel
          pkgs.python3
          pkgs.xvfb-run
          pkgs.pulseaudio
          pkgs.mumble
          pkgs.openssl
          pkgs.iptables
          pkgs.iproute2
          pkgs.procps
        ];
        hardware.graphics.enable = true;
        fonts.packages = [ pkgs.dejavu_fonts ];
        virtualisation.memorySize = 3072;
        virtualisation.cores = 2;
        users.users.tester = {
          isNormalUser = true;
          createHome = true;
        };
      };
  };
  testScript = ''
    def start_server(node):
        node.succeed("install -d -m 0700 /run/keys && printf %s ${superuser} > /run/keys/nuschel-superuser && chmod 0600 /run/keys/nuschel-superuser")
        # Two random 32-byte keys as hex, kept across restarts (a log's keys never change).
        node.succeed("for key in kt-signing kt-vrf; do test -e /run/keys/nuschel-$key || "
                     "(umask 077; od -An -tx1 -N32 /dev/urandom | tr -d ' \\n' > /run/keys/nuschel-$key); done")
        node.succeed("systemctl restart nuschel.service")
        node.wait_for_unit("nuschel.service")
        node.wait_for_open_port(64738)

    def as_tester(command):
        client.succeed(f"su - tester -c '{command}'")

    start_all()
    for node in (server, postgres):
        node.wait_for_unit("multi-user.target")
        start_server(node)
    client.wait_for_unit("multi-user.target")
    client.succeed("printf %s ${superuser} > /tmp/superuser && chmod 0644 /tmp/superuser")
    tests = "${../tests}"
    bins = "${nuschel}/bin"

    # The fixture channels, created over the protocol; they must survive a restart.
    as_tester(f"python3 {tests}/seed_server.py {bins} --host server --password-file /tmp/superuser")
    server.succeed("systemctl restart nuschel.service")
    server.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/seed_server.py {bins} --host server --verify")
    server.succeed("test \"$(stat -c %a /var/lib/private/nuschel/server.sqlite3)\" = 600")

    # The client suites vm-smoke runs against stock Murmur, unchanged.
    as_tester(f"python3 {tests}/mumble_client.py {bins} --host server --opus-library ${opus}")
    client.succeed(f"python3 {tests}/udp_voice.py {bins} --host server --opus-library ${opus} --netem --block-udp")
    as_tester(f"python3 {tests}/protocol_features.py {bins} --host server --opus-library ${opus} --openssl ${pkgs.openssl.bin}/bin/openssl")

    # Nuschel with the extension: stored, acknowledged, read from the server's
    # history after being offline (docs/extension.md).
    as_tester(f"python3 {tests}/nuschel_history.py {bins} --host server")

    # Key transparency (docs/e2ee.md §5): identities published, found, verified,
    # and a key change detected, with the crypto worker in its sandbox; the log
    # (keys from credentials) survives a restart.
    as_tester(f"python3 {tests}/key_transparency.py {bins} --host server")
    server.succeed("systemctl restart nuschel.service")
    server.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/key_transparency.py {bins} --host server")
    server.succeed("test ! -e /var/lib/private/nuschel/kt-vrf.key")

    # The real stock Mumble GUI against nuscheld: login with a certificate
    # exported by Nuschel, then voice in both directions.
    as_tester(f"xvfb-run -a -s \"-screen 0 1280x800x24\" python3 {tests}/stock_client.py {bins} --mumble ${pkgs.mumble}/bin/mumble --host server --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${opus} --output /tmp/nuschel-stock-artifacts")
    client.copy_from_machine("/tmp/nuschel-stock-artifacts", "stock")

    # PostgreSQL: the same persistence through the storage worker.
    as_tester(f"python3 {tests}/seed_server.py {bins} --host postgres --password-file /tmp/superuser")
    postgres.succeed("systemctl restart nuschel.service")
    postgres.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/seed_server.py {bins} --host postgres --verify")
    # The history schema on PostgreSQL, and that it survives a restart.
    as_tester(f"python3 {tests}/nuschel_history.py {bins} --host postgres")
    postgres.succeed("systemctl restart nuschel.service")
    postgres.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/nuschel_history.py {bins} --host postgres")
    # Key transparency on PostgreSQL, with keys the server generated (0600).
    as_tester(f"python3 {tests}/key_transparency.py {bins} --host postgres")
    postgres.succeed("test \"$(stat -c %a /var/lib/private/nuschel/kt-vrf.key)\" = 600")
    print(server.succeed("journalctl -u nuschel.service --no-pager | tail -n 40"))
  '';
}
