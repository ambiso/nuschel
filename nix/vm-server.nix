# nuschld under the NixOS module, with stock clients: the stock Mumble GUI
# (login with a Nuschl-exported certificate, voice and chat both ways) and the same
# client suites vm-smoke runs against stock Murmur; and Nuschl with the
# extension's server history (SQLite and PostgreSQL) and key transparency (its
# keys as credentials on one node, generated on the other). A second node runs
# the server on local PostgreSQL. State must survive service restarts. A third
# runs nuschl-witness behind nginx; the first server submits its epoch log to it
# and clients require its cosignatures (docs/witness.md).
{
  pkgs,
  nuschl,
  serverPackage ? nuschl,
  module,
  witnessModule,
}:
let
  # A test witness key (public: never use it elsewhere) and its verifier key; and
  # another witness's key that the server does not reach.
  witnessSeed = "5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";
  witnessKey = ../tests/fixtures/witness-test.vkey;
  otherWitnessKey = ../tests/fixtures/witness-other.vkey;
  superuser = "nuschl-super";
  opus = "${pkgs.libopus}/lib/libopus.so";
  # The SuperUser password is a runtime file (the module refuses store paths);
  # the test writes it before (re)starting the service.
  serverNode =
    extra:
    { lib, ... }:
    {
      imports = [ module ];
      services.nuschl = {
        enable = true;
        package = serverPackage;
        openFirewall = true;
        superUserPasswordFile = "/run/keys/nuschl-superuser";
        settings = {
          welcome_text = "Welcome to the <b>Nuschl</b> VM server.";
          statistics_interval = 5;
          max_bandwidth = 72000;
          # Negative certificate probes and several test profiles share one client IP.
          autoban.attempts = 0;
          password_hashing = {
            memory_kib = 4096;
            passes = 1;
          };
          # Submissions to the witness every two seconds instead of every minute.
          kt.witness_interval = 2;
        };
      }
      // extra;
      # Started by the test once the password file exists.
      systemd.services.nuschl.wantedBy = lib.mkForce [ ];
      virtualisation.memorySize = 1536;
    };
in
pkgs.testers.runNixOSTest {
  name = "nuschl-server";
  nodes = {
    server =
      { pkgs, ... }:
      {
        imports = [
          (serverNode {
            kt = {
              # Key-transparency keys as credentials (written by the test, never in the store).
              signingKeyFile = "/run/keys/nuschl-kt-signing";
              vrfSecretFile = "/run/keys/nuschl-kt-vrf";
              # Witnessed by the witness node, under the name clients use.
              origins = [ "server:64738" ];
              witnesses = [
                {
                  key = pkgs.lib.removeSuffix "\n" (builtins.readFile witnessKey);
                  url = "http://witness";
                }
              ];
            };
          })
        ];
        # Builds the Murmur fixture database (tests/fixtures/murmur-1.5.sql).
        environment.systemPackages = [ pkgs.sqlite ];
        # The import into a fresh database, switched to at the end of the test.
        specialisation.murmur.configuration.services.nuschl = {
          importMurmur.databaseFile = "/tmp/murmur.sqlite";
          database.sqlitePath = "/var/lib/nuschl/imported.sqlite3";
        };
      };
    postgres = serverNode {
      database.type = "postgresql";
      database.createLocally = true;
    };
    witness =
      { lib, pkgs, ... }:
      {
        imports = [ witnessModule ];
        services.nuschl-witness = {
          enable = true;
          package = nuschl;
          name = "witness/nuschl-kt";
          keyFile = "/run/keys/witness-key";
          virtualHost = "witness";
          openFirewall = true;
          # The test network is private.
          enrolPrivateAddresses = true;
        };
        # Started by the test once the key file exists.
        systemd.services.nuschl-witness.wantedBy = lib.mkForce [ ];
        environment.systemPackages = [ pkgs.curl ];
      };
    encrypted = serverNode { security = "e2ee"; };
    encryptedPostgres = serverNode {
      security = "e2ee";
      database.type = "postgresql";
      database.createLocally = true;
    };
    client =
      { ... }:
      {
        environment.systemPackages = [
          nuschl
          pkgs.python3
          pkgs.xvfb-run
          pkgs.pulseaudio
          pkgs.mumble
          # Typing into the stock window and reading its log (stock_client.py);
          # xwd keeps the screen of a failed run.
          pkgs.xdotool
          pkgs.xclip
          pkgs.xwd
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
        node.succeed("install -d -m 0700 /run/keys && printf %s ${superuser} > /run/keys/nuschl-superuser && chmod 0600 /run/keys/nuschl-superuser")
        # Two random 32-byte keys as hex, kept across restarts (a log's keys never change).
        node.succeed("for key in kt-signing kt-vrf; do test -e /run/keys/nuschl-$key || "
                     "(umask 077; od -An -tx1 -N32 /dev/urandom | tr -d ' \\n' > /run/keys/nuschl-$key); done")
        node.succeed("systemctl restart nuschl.service")
        node.wait_for_unit("nuschl.service")
        node.wait_for_open_port(64738)
        # Exercise the standalone package, not the client's incidental copy.
        node.succeed("readlink -f /proc/$(systemctl show -p MainPID --value nuschl.service)/exe | grep -Fx ${serverPackage}/bin/nuschld")

    def as_tester(command):
        client.succeed(f"su - tester -c '{command}'")

    start_all()
    # The witness first, with its key as a credential: the server submits to it.
    witness.wait_for_unit("multi-user.target")
    witness.succeed("install -d -m 0700 /run/keys && printf %s ${witnessSeed} > /run/keys/witness-key && chmod 0600 /run/keys/witness-key")
    witness.succeed("systemctl start nuschl-witness.service")
    witness.wait_for_unit("nuschl-witness.service")
    witness.wait_for_unit("nginx.service")
    witness.wait_for_open_port(80)
    witness.succeed("curl -sf http://witness/nuschl-kt/v1/vkey | diff - ${witnessKey}")
    witness.succeed("test \"$(stat -c %a /var/lib/private/nuschl-witness)\" = 700")
    for node in (server, postgres, encrypted, encryptedPostgres):
        node.wait_for_unit("multi-user.target")
        start_server(node)
    client.wait_for_unit("multi-user.target")
    client.succeed("printf %s ${superuser} > /tmp/superuser && chmod 0644 /tmp/superuser")
    tests = "${../tests}"
    bins = "${nuschl}/bin"

    # The fixture channels, created over the protocol; they must survive a restart.
    as_tester(f"python3 {tests}/seed_server.py {bins} --host server --password-file /tmp/superuser")
    server.succeed("systemctl restart nuschl.service")
    server.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/seed_server.py {bins} --host server --verify")
    server.succeed("test \"$(stat -c %a /var/lib/private/nuschl/server.sqlite3)\" = 600")

    # The client suites vm-smoke runs against stock Murmur, unchanged.
    as_tester(f"python3 {tests}/mumble_client.py {bins} --host server --opus-library ${opus}")
    client.succeed(f"python3 {tests}/udp_voice.py {bins} --host server --opus-library ${opus} --netem --block-udp")
    as_tester(f"python3 {tests}/protocol_features.py {bins} --host server --opus-library ${opus} --openssl ${pkgs.openssl.bin}/bin/openssl")

    # Nuschl with the extension: stored, acknowledged, read from the server's
    # history after being offline (docs/extension.md).
    as_tester(f"python3 {tests}/nuschl_history.py {bins} --host server")

    # Key transparency (docs/e2ee.md §5): identities published, found, verified,
    # and a key change detected, with the crypto worker in its sandbox; the log
    # (keys from credentials) survives a restart.
    as_tester(f"python3 {tests}/key_transparency.py {bins} --host server")
    server.succeed("systemctl restart nuschl.service")
    server.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/key_transparency.py {bins} --host server")
    server.succeed("test ! -e /var/lib/private/nuschl/kt-vrf.key")

    # Witnessing (docs/witness.md): the server enrolled with the witness (which
    # read the server's hello), submits its epochs, and clients that require the
    # witness see its cosignatures; a witness the server does not reach is
    # "unavailable". The witness keeps its state across a restart.
    as_tester(f"python3 {tests}/kt_witness.py {bins} --host server --witness-key ${witnessKey} --other-key ${otherWitnessKey}")
    witness.succeed("journalctl -u nuschl-witness.service --no-pager | grep -q 'enrolled a log'")
    server.wait_until_succeeds("journalctl -u nuschl.service --no-pager | grep -q 'a witness cosigns this server'", timeout=30)
    witness.succeed("curl -sf http://witness/nuschl-kt/v1/checkpoints/$(printf %s server:64738/nuschl-kt/v1 | sha256sum | cut -c1-64) | grep -q '^server:64738/nuschl-kt/v1$'")
    witness.succeed("systemctl restart nuschl-witness.service")
    witness.wait_for_open_port(8473)
    as_tester(f"python3 {tests}/kt_witness.py {bins} --host server --witness-key ${witnessKey} --other-key ${otherWitnessKey}")

    # End-to-end-encrypted direct conversations (mls/1, docs/e2ee.md "Phase 4
    # prototype"): group creation, messages both ways, offline delivery, and the
    # group and stream surviving a restart.
    as_tester(f"python3 {tests}/e2ee_direct.py {bins} --host encrypted --state /tmp/nuschl-e2ee")
    encrypted.succeed("systemctl restart nuschl.service")
    encrypted.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/e2ee_direct.py {bins} --host encrypted --state /tmp/nuschl-e2ee --phase second")

    # The real stock Mumble GUI against nuschld: login with a certificate
    # exported by Nuschl, then voice and chat in both directions.
    as_tester(f"xvfb-run -a -s \"-screen 0 1280x800x24\" python3 {tests}/stock_client.py {bins} --mumble ${pkgs.mumble}/bin/mumble --host server --pulse-bin ${pkgs.pulseaudio}/bin --opus-library ${opus} --output /tmp/nuschl-stock-artifacts")
    client.copy_from_machine("/tmp/nuschl-stock-artifacts", "stock")

    # PostgreSQL: the same persistence through the storage worker.
    as_tester(f"python3 {tests}/seed_server.py {bins} --host postgres --password-file /tmp/superuser")
    postgres.succeed("systemctl restart nuschl.service")
    postgres.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/seed_server.py {bins} --host postgres --verify")
    # The history schema on PostgreSQL, and that it survives a restart.
    as_tester(f"python3 {tests}/nuschl_history.py {bins} --host postgres")
    postgres.succeed("systemctl restart nuschl.service")
    postgres.wait_for_open_port(64738)
    as_tester(f"python3 {tests}/nuschl_history.py {bins} --host postgres")
    # Key transparency on PostgreSQL, with keys the server generated (0600).
    as_tester(f"python3 {tests}/key_transparency.py {bins} --host postgres")
    # End-to-end direct conversations on PostgreSQL.
    as_tester(f"python3 {tests}/e2ee_direct.py {bins} --host encryptedPostgres")
    postgres.succeed("test \"$(stat -c %a /var/lib/private/nuschl/kt-vrf.key)\" = 600")
    # The periodic statistics line.
    server.wait_until_succeeds("journalctl -u nuschl.service --no-pager | grep -q 'statistics'", timeout=30)

    # SIGHUP reloads the (generated) certificate; the server keeps running.
    server.succeed("systemctl kill -s HUP nuschl.service")
    server.wait_until_succeeds("journalctl -u nuschl.service --no-pager | grep -q 'reloaded the server certificate'", timeout=30)
    server.succeed("systemctl is-active nuschl.service")

    # A Murmur database (Murmur's own schema, tests/fixtures/murmur-1.5.sql)
    # imported by the module into a fresh SQLite database, then logged in to.
    server.succeed(f"sqlite3 /tmp/murmur.sqlite < {tests}/fixtures/murmur-1.5.sql && chmod 0600 /tmp/murmur.sqlite")
    # Where Landlock is unavailable (its syscalls fail with ENOSYS here) the reader
    # refuses the file; --allow-unconfined-murmur-read reads it, and the report says so.
    unconfined = (
        "systemd-run --wait --pipe --collect -p SystemCallErrorNumber=ENOSYS"
        " -p SystemCallFilter='~landlock_create_ruleset landlock_add_rule landlock_restrict_self'"
        " ${serverPackage}/bin/nuschld --state-directory /tmp/unconfined"
        " admin import-murmur /tmp/murmur.sqlite"
    )
    refused = server.fail(f"{unconfined} 2>&1")
    assert "reader: not confined (no Landlock in this kernel" in refused, refused
    assert "the file was not read" in refused, refused
    allowed = server.succeed(f"{unconfined} --allow-unconfined-murmur-read 2>&1")
    assert "read anyway (--allow-unconfined-murmur-read)" in allowed, allowed
    server.succeed("/run/current-system/specialisation/murmur/bin/switch-to-configuration test")
    server.wait_until_succeeds("test -e /var/lib/nuschl/murmur-imported", timeout=120)
    server.wait_for_unit("nuschl.service")
    server.wait_for_open_port(64738)
    server.succeed("test ! -e /var/lib/nuschl/murmur-import.sqlite")
    server.succeed("test \"$(stat -c %a /var/lib/nuschl/murmur-server-password)\" = 600")
    # The report (with whether Landlock confined the reader) is in the import unit's journal.
    print(server.succeed("journalctl -u nuschl-import-murmur.service --no-pager | grep -A45 'imported from Murmur'"))
    as_tester(f"python3 {tests}/murmur_import.py {bins} --host server")
    print(server.succeed("journalctl -u nuschl.service --no-pager | tail -n 40"))
  '';
}
