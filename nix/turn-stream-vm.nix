# The screen-share TURN relay and stream host under their modules
# (nix/turn-module.nix, nix/stream-module.nix), driven by coturn's own
# turnutils. coturn relays only to the fake SFU peer, and there only to its
# allowed port; other hosts and loopback are refused. REST credentials work
# until they expire, the per-user quota and no-tcp-relay hold, TLS works.
# The stream host sends its isolation headers and no cookies. No ACME here.
{
  pkgs,
  turnModule,
  streamModule,
}:
let
  site = pkgs.runCommand "nuschl-stream-test-site" { } ''
    mkdir -p "$out"
    echo '<!doctype html><title>watch page</title>' > "$out/watch.html"
    echo '<!doctype html><title>share page</title>' > "$out/share.html"
    echo 'console.log("asset")' > "$out/app.js"
  '';
  # turnutils_peer echoes UDP on every address of its host.
  peer = port: {
    wantedBy = [ "multi-user.target" ];
    serviceConfig.ExecStart = "${pkgs.coturn}/bin/turnutils_peer -p ${toString port}";
  };
in
pkgs.testers.runNixOSTest {
  name = "nuschl-turn-stream";
  nodes = {
    turn =
      { nodes, ... }:
      {
        imports = [
          turnModule
          streamModule
        ];
        networking.nftables.enable = true;
        networking.firewall.allowedTCPPorts = [ 80 ];
        # Test credentials, made at boot.
        systemd.services.turn-test-credentials = {
          requiredBy = [ "coturn.service" ];
          before = [ "coturn.service" ];
          path = [ pkgs.openssl ];
          serviceConfig = {
            Type = "oneshot";
            UMask = "0077";
          };
          script = ''
            mkdir -p /var/lib/turn-test && cd /var/lib/turn-test
            [ -e rest-secret ] || openssl rand -hex 32 | tr -d '\n' > rest-secret
            [ -e key.pem ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
              -nodes -days 2 -subj /CN=turn.test -keyout key.pem -out cert.pem 2>/dev/null
          '';
        };
        services.nuschl-turn = {
          enable = true;
          hostName = "turn.test";
          secretFile = "/var/lib/turn-test/rest-secret";
          allowedPeerAddresses = [
            nodes.sfu.networking.primaryIPAddress
            nodes.sfu.networking.primaryIPv6Address
          ];
          allowedPeerPorts = [ 40000 ];
          userQuota = 2;
          openFirewall = true;
          tls = {
            certificateFile = "/var/lib/turn-test/cert.pem";
            keyFile = "/var/lib/turn-test/key.pem";
          };
        };
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.test";
          enableACME = false;
          forceSSL = false;
          inherit site;
        };
      };
    sfu = {
      networking.firewall.allowedUDPPorts = [
        40000
        40001
      ];
      systemd.services.peer = peer 40000;
      systemd.services.peer-other-port = peer 40001;
    };
    client = {
      environment.systemPackages = [
        pkgs.coturn
        pkgs.curl
      ];
    };
  };
  testScript =
    { nodes, ... }:
    let
      turn4 = nodes.turn.networking.primaryIPAddress;
      turn6 = nodes.turn.networking.primaryIPv6Address;
      sfu4 = nodes.sfu.networking.primaryIPAddress;
      sfu6 = nodes.sfu.networking.primaryIPv6Address;
      client4 = nodes.client.networking.primaryIPAddress;
      client6 = nodes.client.networking.primaryIPv6Address;
    in
    ''
      import base64, hashlib, hmac, re

      start_all()
      turn.wait_for_unit("coturn.service")
      turn.wait_for_unit("nginx.service")
      sfu.wait_for_unit("peer.service")
      sfu.wait_for_unit("peer-other-port.service")
      client.wait_for_unit("multi-user.target")

      secret = turn.succeed("cat /var/lib/turn-test/rest-secret").encode()
      now = int(turn.succeed("date +%s"))

      def relay(user, peer, *flags, port=40000, lifetime=600, key=secret, server="${turn4}", expect=0):
          """Sends packets to peer:port through TURN with REST credentials; returns uclient's output."""
          username = f"{now + lifetime}:{user}"
          password = base64.b64encode(hmac.new(key, username.encode(), hashlib.sha1).digest()).decode()
          status, out = client.execute(
              f"timeout 60 turnutils_uclient --no-even-port -c -n 3 -u '{username}' -w '{password}' "
              f"-e {peer} -r {port} {' '.join(flags)} {server} 2>&1"
          )
          print(out)
          assert status == expect, (status, out)
          return out

      def echoed(out):
          """(sent, received) from uclient's final count."""
          sent, received = re.findall(r"tot_send_msgs=(\d+), tot_recv_msgs=(\d+)\n", out)[-1]
          return int(sent), int(received)

      with subtest("only the SFU's allowed port is a peer"):
          assert echoed(relay("v1", "${sfu4}")) == (3, 3)
          assert echoed(relay("v2", "${sfu4}", port=40001)) == (3, 0)
          turn.succeed("nft list table inet nuschl-turn-peers | grep -q 'counter packets [1-9]'")
          for denied in ["${client4}", "127.0.0.1", "10.0.0.1"]:
              assert "error 403" in relay("v3-" + denied, denied, expect=255)

      with subtest("IPv6: only the SFU is a peer"):
          assert echoed(relay("v6", "${sfu6}", "-x", server="${turn6}")) == (3, 3)
          assert "error 403" in relay("v6-denied", "${client6}", "-x", server="${turn6}", expect=255)

      with subtest("expired and forged credentials are refused"):
          assert "Cannot complete Allocation" in relay("expired", "${sfu4}", lifetime=-60, expect=255)
          assert "Cannot complete Allocation" in relay("forged", "${sfu4}", key=b"0" * 64, expect=255)
          logs = turn.succeed("journalctl --namespace=nuschl-turn -o cat")
          assert ":expired>" in logs and ":forged>" in logs, logs

      with subtest("per-user quota, no TCP relays, no anonymous STUN, no CLI"):
          relay("quota", "${sfu4}")
          assert "error 486" in relay("quota", "${sfu4}", expect=255)
          relay("other-user", "${sfu4}")
          assert "error 442" in relay("tcp", "${sfu4}", "-T", expect=255)
          assert "error 401" in client.execute("timeout 10 turnutils_stunclient ${turn4} 2>&1")[1]
          turn.fail("ss -ltn | grep -q ':5766 '")

      with subtest("TURN over TLS"):
          client.succeed("cat > /tmp/turn-ca.pem <<'EOF'\n" + turn.succeed("cat /var/lib/turn-test/cert.pem") + "EOF")
          assert echoed(relay("tls", "${sfu4}", "-t", "-S", "-E", "/tmp/turn-ca.pem")) == (3, 3)

      with subtest("coturn's journal keeps admitted users and client addresses out"):
          logs = turn.succeed("journalctl --namespace=nuschl-turn -o cat")
          for private in [":v1>", ":tls>", "${client4}"]:
              assert not any(private in line for line in logs.splitlines() if "peer IP" not in line), (private, logs)

      with subtest("stream host: headers on every location, no cookies, no access log"):
          def head(path):
              out = client.succeed(f"curl -sS -o /dev/null -D - --resolve stream.test:80:${turn4} http://stream.test{path}").lower()
              print(out)
              for header in ["content-security-policy:", "cross-origin-opener-policy: same-origin",
                             "cross-origin-embedder-policy: require-corp", "cross-origin-resource-policy: same-origin",
                             "origin-agent-cluster: ?1", "publickey-credentials-get=()",
                             "referrer-policy: no-referrer", "x-content-type-options: nosniff"]:
                  assert header in out, (path, header)
              assert "set-cookie" not in out, (path, out)
              return out

          watch = head("/watch")
          assert watch.startswith("http/1.1 200") and "display-capture=()" in watch
          assert "display-capture=(self)" in head("/share")
          assert head("/app.js").startswith("http/1.1 200")
          for missing in ["/", "/missing", "/sfu"]:
              assert head(missing).startswith("http/1.1 404"), missing
          turn.fail("grep -rqE '/watch|/share' /var/log/nginx/")
    '';
}
