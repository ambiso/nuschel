{
  pkgs,
  musicbot,
  cli,
  module,
}:
let
  # tests/fixtures/README.md: the test server's certificate, pinned here.
  pin = "d216518fdf25aa0b1c57881888c4304fb2bbbd4bbd763f267ee75b3d9445d1d8";
  binaries = pkgs.symlinkJoin {
    name = "nuschl-musicbot-test-binaries";
    paths = [
      musicbot
      cli
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "nuschl-musicbot";
  nodes.machine =
    { ... }:
    {
      imports = [ module ];
      services.murmur = {
        enable = true;
        tls.certPath = "${../tests/fixtures/musicbot-murmur.crt}";
        tls.keyPath = "${../tests/fixtures/musicbot-murmur.key}";
        # The probes for the pin and the listeners share one address.
        autobanAttempts = 0;
      };
      systemd.services.fake-library = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          DynamicUser = true;
          ExecStart = "${pkgs.python3}/bin/python3 ${../tests}/fake_subsonic.py --port 4533";
        };
      };
      # Test secrets; real ones stay out of the Nix store.
      environment.etc."musicbot/password".text = "library-secret\n";
      environment.etc."musicbot/key".text = "open-key-123\n";
      services.nuschl-musicbot = {
        enable = true;
        package = musicbot;
        credentials = {
          plain = "/etc/musicbot/password";
          open = "/etc/musicbot/key";
        };
        settings = {
          subsonic = [
            {
              name = "Plain";
              url = "http://127.0.0.1:4533/plain";
              username = "bot";
              password_file = "plain";
            }
            {
              name = "Open";
              url = "http://127.0.0.1:4533/open";
              api_key_file = "open";
            }
          ];
          channel = [
            {
              name = "root";
              host = "127.0.0.1";
              username = "DJ";
              certificate_sha256 = pin;
            }
          ];
        };
      };
      systemd.services.nuschl-musicbot = {
        after = [
          "murmur.service"
          "fake-library.service"
        ];
      };
      environment.systemPackages = [
        binaries
        pkgs.python3
        pkgs.curl
        pkgs.util-linux
      ];
      # The fixture needs only its own guest loopback services, never Internet.
      virtualisation.restrictNetwork = true;
      services.timesyncd.enable = false;
      networking.timeServers = [ ];
      virtualisation.memorySize = 2048;
      virtualisation.cores = 2;
    };
  testScript = ''
    machine.wait_for_unit("murmur.service")
    machine.wait_for_open_port(64738)
    machine.wait_for_open_port(4533)
    machine.wait_for_unit("nuschl-musicbot.service")
    machine.wait_for_open_port(8474)
    machine.wait_until_succeeds("journalctl -u nuschl-musicbot | grep -q 'root: connected'")
    machine.succeed("journalctl -u nuschl-musicbot | grep -q 'Open: OpenSubsonic extensions'")
    # The client certificate lives in the private state directory.
    machine.succeed("test -d /var/lib/private/nuschl-musicbot/identities/root")
    machine.succeed("curl -fs http://127.0.0.1:8474/ | grep -q '!web'")
    headers = machine.succeed("curl -s -D - -o /dev/null http://127.0.0.1:8474/u/00000000-0000-0000-0000-000000000000/")
    assert " 404 " in headers.splitlines()[0], headers
    assert "referrer-policy: no-referrer" in headers.lower(), headers
    # The module's bot: a link, two songs from both libraries, heard in the root channel.
    machine.succeed("python3 ${../tests}/musicbot.py ${binaries}/bin 64738 --service")
    # Inspect actual child capabilities under DynamicUser + the outer syscall
    # and namespace restrictions, after the service has encoded real music.
    machine.succeed("python3 ${../tests}/musicbot_service_isolation.py")
    # Recovered faults of the service's own positively identified duck worker
    # (stopped past its deadline, then killed mid-request) keep the same TLS
    # session: no reconnection, music heard, !web answered, worker reaped.
    print(machine.succeed("python3 ${../tests}/musicbot_duck_faults.py ${binaries}/bin 64738 --service --espeak ${pkgs.espeak-ng}/bin/espeak-ng --opus-library ${pkgs.libopus}/lib/libopus.so"))
    # The program on its own: two channels, shared and separate queues, and
    # ducking for real speech (espeak-ng, encoded by reference libopus).
    machine.succeed("python3 ${../tests}/with_local_murmur.py ${pkgs.murmur}/bin/mumble-server ${../tests}/musicbot.py ${binaries}/bin PORT --espeak ${pkgs.espeak-ng}/bin/espeak-ng --opus-library ${pkgs.libopus}/lib/libopus.so")
  '';
}
