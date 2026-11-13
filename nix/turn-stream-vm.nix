# The screen-share TURN relay and stream host under their NixOS modules
# (nix/turn-module.nix, nix/stream-module.nix). coturn relays only to the fake
# SFU peer, and only to its allowed port; loopback, private and its own
# addresses are refused. REST credentials work until they expire, quotas and
# no-tcp-relay hold, TLS presents the runtime certificate. The stream host
# sends the isolation headers, sets no cookies and proxies /sfu to a fake
# socket-activated SFU under DynamicUser. No secret reaches the Nix store:
# the module refuses store paths at evaluation, and the runtime secret is in
# no file of the system's store closure. No ACME in the VM.
{
  pkgs,
  turnModule,
  streamModule,
}:
let
  inherit (pkgs) lib;
  probe = pkgs.writeShellScriptBin "turn-probe" ''
    exec ${pkgs.python3}/bin/python3 ${../tests/turn_stream_probe.py} "$@"
  '';
  site = pkgs.runCommand "nuschl-stream-test-site" { } ''
    mkdir -p "$out"
    echo '<!doctype html><title>watch page</title>' > "$out/watch.html"
    echo '<!doctype html><title>share page</title>' > "$out/share.html"
    echo 'console.log("asset")' > "$out/app.js"
  '';
  # Answers each signalling connection with what it received and what it was
  # given, so the test can check the proxy and the unit's sandbox.
  fakeSfu = pkgs.writeScriptBin "nuschl-sfu" ''
    #!${pkgs.python3}/bin/python3
    import hashlib, json, os, socket, sys
    assert os.environ.get("LISTEN_FDS") == "1", "not socket-activated"
    config = open(sys.argv[sys.argv.index("--config") + 1]).read()
    credentials = os.environ.get("CREDENTIALS_DIRECTORY")
    secret = open(os.path.join(credentials, "turn-rest-secret"), "rb").read().strip(b"\n")
    def attempt(kind, port, target=None):
        sock = socket.socket(socket.AF_INET, kind)
        try:
            sock.bind(("0.0.0.0", port))
            if target:
                sock.sendto(b"probe", target)
            return "ok"
        except OSError as error:
            return error.strerror
        finally:
            sock.close()
    probes = {
        "bind udp media": attempt(socket.SOCK_DGRAM, 40443),
        "bind udp other": attempt(socket.SOCK_DGRAM, 40444),
        "bind tcp media": attempt(socket.SOCK_STREAM, 40443),
        "send private": attempt(socket.SOCK_DGRAM, 40443, ("192.168.1.2", 40000)),
        "send loopback": attempt(socket.SOCK_DGRAM, 40443, ("127.0.0.1", 40000)),
    }
    server = socket.socket(fileno=3)
    while True:
        connection, _ = server.accept()
        with connection:
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = connection.recv(4096)
                if not chunk:
                    break
                data += chunk
            lines = data.split(b"\r\n\r\n")[0].decode(errors="replace").split("\r\n")
            headers = {}
            for line in lines[1:]:
                name, _, value = line.partition(":")
                headers[name.strip().lower()] = value.strip()
            body = json.dumps({
                "request": lines[0],
                "headers": headers,
                "uid": os.getuid(),
                "secret_sha256": hashlib.sha256(secret).hexdigest(),
                "probes": probes,
                "config": config,
            }).encode()
            if headers.get("upgrade", "").lower() == "websocket":
                head = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            else:
                head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n" % len(body)
            connection.sendall(head + b"Set-Cookie: tracking=1\r\n\r\n" + body)
  '';
  # Bound to the given addresses only; retried until IPv6 addresses are usable.
  echo = addresses: ports: {
    description = "UDP echo peer";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ];
    serviceConfig = {
      ExecStart = "${probe}/bin/turn-probe echo ${
        lib.concatMapStringsSep " " (address: "--bind ${address}") addresses
      } ${lib.concatMapStringsSep " " (port: "--port ${toString port}") ports}";
      Restart = "on-failure";
      RestartSec = "1s";
    };
  };

  # Evaluation-time checks: misconfigurations the modules must refuse.
  failingAssertions =
    modules:
    let
      system = import (pkgs.path + "/nixos/lib/eval-config.nix") {
        inherit (pkgs.stdenv.hostPlatform) system;
        modules = [
          turnModule
          streamModule
          {
            system.stateVersion = "26.05";
            boot.loader.grub.enable = false;
            fileSystems."/" = {
              device = "/dev/null";
              fsType = "ext4";
            };
          }
        ]
        ++ modules;
      };
      # Only failed assertions' messages are evaluated (as NixOS does).
      failed = map (assertion: assertion.message) (
        lib.filter (assertion: !assertion.assertion) system.config.assertions
      );
    in
    lib.filter (
      message:
      lib.hasPrefix "services.nuschl-" message || lib.hasPrefix "Set the TURN REST secret" message
    ) failed;
  turnBase = {
    enable = true;
    hostName = "turn.example.org";
    secretFile = "/run/agenix/turn";
  };
  refuses =
    name: expected: modules:
    let
      messages = failingAssertions modules;
    in
    lib.throwIfNot (lib.any (lib.hasInfix expected) messages)
      "turn-stream eval check '${name}' expected an assertion containing '${expected}', got ${builtins.toJSON messages}"
      true;
  accepts =
    name: modules:
    let
      messages = failingAssertions modules;
    in
    lib.throwIfNot (
      messages == [ ]
    ) "turn-stream eval check '${name}' expected no assertion, got ${builtins.toJSON messages}" true;
  peerRefused =
    address:
    refuses "non-public peer ${address}" "set allowNonPublicPeers only for test networks" [
      {
        services.nuschl-turn = turnBase // {
          allowedPeerAddresses = [ address ];
        };
      }
    ];
  evalChecks = [
    (refuses "secret in the store" "outside the Nix store" [
      {
        services.nuschl-turn = turnBase // {
          secretFile = "${pkgs.writeText "turn-secret" "not-a-secret"}";
        };
      }
    ])
    (refuses "coturn static secret" "never services.coturn.static-auth-secret" [
      {
        services.nuschl-turn = turnBase;
        services.coturn.static-auth-secret = "not-a-secret";
      }
    ])
    (refuses "peer range" "single IPv4 or IPv6 addresses" [
      {
        services.nuschl-turn = turnBase // {
          allowedPeerAddresses = [ "1.2.3.0-1.2.3.255" ];
        };
      }
    ])
    (refuses "peer ports without nftables" "enable networking.nftables" [
      {
        services.nuschl-turn = turnBase // {
          allowedPeerAddresses = [ "1.1.1.1" ];
          allowedPeerPorts = [ 443 ];
        };
      }
    ])
    (refuses "SFU without a package" "requires services.nuschl-stream.sfu.package" [
      {
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.example.org";
          sfu.enable = true;
        };
      }
    ])
    (refuses "SFU secret in the store" "outside the Nix store" [
      {
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.example.org";
          sfu.turn.secretFile = "${pkgs.writeText "turn-secret" "not-a-secret"}";
        };
      }
    ])
    (accepts "public peers and a disabled SFU" [
      {
        networking.nftables.enable = true;
        services.nuschl-turn = turnBase // {
          allowedPeerAddresses = [
            "1.1.1.1"
            "2606:4700::1111"
          ];
          allowedPeerPorts = [ 443 ];
        };
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.example.org";
        };
      }
    ])
  ]
  ++ map peerRefused [
    "127.0.0.1"
    "10.1.2.3"
    "100.64.0.1"
    "169.254.1.1"
    "172.20.0.1"
    "192.168.1.1"
    "224.0.0.1"
    "203.0.113.5"
    "::1"
    "::ffff:1.1.1.1"
    "fe80::1"
    "fd00::1"
    "ff02::1"
    "2001:db8::1"
  ];
in
assert lib.all lib.id evalChecks;
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
        environment.systemPackages = [ pkgs.socat ];
        # Test credentials, generated at runtime (never in the store).
        systemd.services.turn-test-credentials = {
          wantedBy = [ "multi-user.target" ];
          requiredBy = [ "coturn.service" ];
          before = [
            "coturn.service"
            "nuschl-sfu.service"
          ];
          path = [ pkgs.openssl ];
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            UMask = "0077";
          };
          script = ''
            mkdir -p /var/lib/turn-test
            cd /var/lib/turn-test
            [ -e rest-secret ] || openssl rand -hex 32 | tr -d '\n' > rest-secret
            [ -e key.pem ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
              -nodes -days 2 -subj /CN=turn.test -addext subjectAltName=DNS:turn.test \
              -keyout key.pem -out cert.pem 2>/dev/null
          '';
        };
        # Local services a relay must not reach.
        systemd.services.echo =
          echo
            [
              "127.0.0.1"
              "::1"
              nodes.turn.networking.primaryIPAddress
            ]
            [ 40000 ];
        services.nuschl-turn = {
          enable = true;
          hostName = "turn.test";
          secretFile = "/var/lib/turn-test/rest-secret";
          allowedPeerAddresses = [
            nodes.sfu.networking.primaryIPAddress
            nodes.sfu.networking.primaryIPv6Address
          ];
          allowedPeerPorts = [ 40000 ];
          # The test network is private and IPv6 documentation space.
          allowNonPublicPeers = true;
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
          openFirewall = true;
          sfu = {
            enable = true;
            package = fakeSfu;
            mediaPort = 40443;
            publicAddresses = [ nodes.turn.networking.primaryIPAddress ];
            turn = {
              uris = [ "turn:turn.test:3478?transport=udp" ];
              secretFile = "/var/lib/turn-test/rest-secret";
            };
          };
        };
      };
    sfu =
      { config, ... }:
      {
        networking.firewall.allowedUDPPorts = [
          40000
          40001
        ];
        systemd.services.echo =
          echo
            [
              config.networking.primaryIPAddress
              config.networking.primaryIPv6Address
            ]
            [
              40000
              40001
            ];
      };
    client =
      { config, ... }:
      {
        environment.systemPackages = [
          probe
          pkgs.curl
          pkgs.python3
        ];
        networking.firewall.allowedUDPPorts = [ 40000 ];
        systemd.services.echo =
          echo
            [
              config.networking.primaryIPAddress
              config.networking.primaryIPv6Address
            ]
            [ 40000 ];
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
      import base64, hashlib, hmac, json

      start_all()
      turn.wait_for_unit("coturn.service")
      turn.wait_for_unit("nginx.service")
      turn.wait_for_unit("nuschl-sfu.socket")
      turn.wait_for_open_port(3478)
      turn.wait_for_open_port(5349)
      turn.wait_for_open_port(80)
      sfu.wait_for_unit("echo.service")
      client.wait_for_unit("echo.service")
      client.wait_for_unit("multi-user.target")

      secret = turn.succeed("cat /var/lib/turn-test/rest-secret").strip()
      assert len(secret) == 64, "unexpected test secret"
      now = int(turn.succeed("date +%s"))

      def rest(user, lifetime, key=secret):
          username = f"{now + lifetime}:{user}"
          digest = hmac.new(key.encode(), username.encode(), hashlib.sha1).digest()
          return username, base64.b64encode(digest).decode()

      def probe(*args):
          out = client.succeed("turn-probe " + " ".join(args))
          print(out)
          return json.loads(out)

      def allocate(username, password, *args, server="${turn4}"):
          return probe("allocate", "--server", server, "--username", f"'{username}'", "--password", f"'{password}'", *args)

      with subtest("anonymous STUN binding is refused (secure-stun)"):
          assert probe("binding", "--server", "${turn4}")["binding"].startswith("error 401")

      with subtest("REST credentials relay to the SFU's allowed port only"):
          user, password = rest("viewer-probe", 600)
          out = allocate(user, password,
              "--peer", "${sfu4}:40000", "--peer", "${sfu4}:40001",
              "--peer", "${client4}:40000", "--peer", "127.0.0.1:40000",
              "--peer", "${turn4}:40000", "--peer", "10.0.0.1:40000")
          assert out["allocations"][0]["result"] == "ok", out
          peers = out["peers"]
          assert peers["${sfu4}:40000"] == {"permission": "ok", "echo": True}, peers
          # The address is allowed, but nftables drops other ports.
          assert peers["${sfu4}:40001"] == {"permission": "ok", "echo": False}, peers
          for denied in ["${client4}:40000", "127.0.0.1:40000", "${turn4}:40000", "10.0.0.1:40000"]:
              assert peers[denied]["permission"].startswith("error 403"), (denied, peers)
          sfu.succeed("journalctl -u echo.service | grep -q 'port=40000 from=.*${turn4}'")
          sfu.fail("journalctl -u echo.service | grep -q 'port=40001 from'")
          client.fail("journalctl -u echo.service | grep -q 'port=40000 from'")
          turn.fail("journalctl -u echo.service | grep -q 'port=40000 from'")
          turn.succeed("nft list table inet nuschl-turn-peers | grep -q 'counter packets [1-9]'")

      with subtest("IPv6: only the SFU is a peer"):
          user, password = rest("viewer-probe6", 600)
          out = allocate(user, password, "--family", "6",
              "--peer", "[${sfu6}]:40000", "--peer", "[${client6}]:40000", "--peer", "[::1]:40000",
              server="${turn6}")
          assert out["allocations"][0]["result"] == "ok", out
          peers = out["peers"]
          assert peers["[${sfu6}]:40000"] == {"permission": "ok", "echo": True}, peers
          for denied in ["[${client6}]:40000", "[::1]:40000"]:
              assert peers[denied]["permission"].startswith("error 403"), (denied, peers)

      with subtest("expired and forged credentials are refused"):
          user, password = rest("viewer-expired", -60)
          assert allocate(user, password)["allocations"][0]["result"].startswith("error 401")
          user, password = rest("viewer-forged", 600, key="0" * 64)
          assert allocate(user, password)["allocations"][0]["result"].startswith("error 401")

      with subtest("per-user allocation quota"):
          user, password = rest("viewer-quota", 600)
          results = [a["result"] for a in allocate(user, password, "--allocations", "3")["allocations"]]
          assert results[:2] == ["ok", "ok"] and results[2].startswith("error 486"), results

      with subtest("no TCP relays"):
          user, password = rest("viewer-tcp", 600)
          result = allocate(user, password, "--transport", "tcp", "--relay-transport", "tcp")["allocations"][0]["result"]
          assert result.startswith("error 442"), result

      with subtest("TURN over TLS with the runtime certificate"):
          certificate = turn.succeed("cat /var/lib/turn-test/cert.pem")
          client.succeed(f"echo '{certificate}' > /tmp/turn-ca.pem")
          user, password = rest("viewer-tls", 600)
          out = allocate(user, password, "--transport", "tls", "--cafile", "/tmp/turn-ca.pem",
              "--sni", "turn.test", "--peer", "${sfu4}:40000")
          assert out["allocations"][0]["result"] == "ok", out
          assert out["peers"]["${sfu4}:40000"] == {"permission": "ok", "echo": True}, out

      with subtest("coturn: no CLI, configuration and logs keep the secret private"):
          turn.fail("ss -ltn | grep -q ':5766 '")
          runtime = turn.succeed("cat /run/coturn/turnserver.cfg")
          assert f"static-auth-secret={secret}" in runtime
          assert "cli" not in runtime.splitlines(), runtime
          for line in ["no-tcp-relay", "denied-peer-ip=0.0.0.0-255.255.255.255",
                       "allowed-peer-ip=${sfu4}", "max-bps=500000", "user-quota=2", "log-min-level=warning"]:
              assert line in runtime.splitlines(), line
          assert turn.succeed("stat -c '%a %U' /run/coturn/turnserver.cfg /run/coturn").split() == ["640", "turnserver", "700", "turnserver"]
          logs = turn.succeed("journalctl --namespace=nuschl-turn --no-pager -o cat")
          print(logs)
          # Refused peer targets are logged (abuse signal); client addresses
          # and admitted sessions' usernames are not.
          kept = [line for line in logs.splitlines() if "denied in the range" not in line]
          for private in [secret, "viewer-probe", "viewer-tls", "viewer-quota", "${client4}", "${client6}"]:
              assert not any(private in line for line in kept), (private, logs)
          turn.fail(f"journalctl --no-pager | grep -qF {secret}")

      with subtest("stream host: headers, pages, no cookies, no access log"):
          def head(path, *args):
              out = client.succeed(f"curl -sS -o /dev/null -D - --resolve stream.test:80:${turn4} {' '.join(args)} http://stream.test{path}")
              print(out)
              return out.lower()

          csp = "content-security-policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self' ws://stream.test; img-src 'self' blob:; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; require-trusted-types-for 'script'"
          common = [csp, "cross-origin-opener-policy: same-origin", "cross-origin-embedder-policy: require-corp",
                    "cross-origin-resource-policy: same-origin", "origin-agent-cluster: ?1",
                    "referrer-policy: no-referrer", "x-content-type-options: nosniff"]
          for path, status, permissions in [
              ("/watch", "200", "display-capture=(), camera=(), microphone=()"),
              ("/share", "200", "display-capture=(self), camera=(), microphone=()"),
              ("/app.js", "200", "display-capture=()"),
              ("/", "404", "display-capture=()"),
              ("/missing", "404", "display-capture=()"),
          ]:
              out = head(path)
              assert out.startswith(f"http/1.1 {status}"), (path, out)
              for header in common:
                  assert header in out, (path, header)
              assert "permissions-policy: " + permissions in out, (path, out)
              assert "publickey-credentials-get=(), publickey-credentials-create=()" in out, (path, out)
              assert "set-cookie" not in out, (path, out)
          assert "watch page" in client.succeed("curl -sS --resolve stream.test:80:${turn4} http://stream.test/watch")

          raw = client.succeed("curl -sS -i --resolve stream.test:80:${turn4} -H 'Cookie: session=1' -H 'Authorization: Bearer x' -H 'Origin: http://stream.test' http://stream.test/sfu")
          print(raw)
          assert "set-cookie" not in raw.lower(), raw
          seen = json.loads(raw.split("\r\n\r\n", 1)[1])
          assert "cookie" not in seen["headers"] and "authorization" not in seen["headers"], seen
          assert seen["headers"]["origin"] == "http://stream.test", seen
          assert seen["headers"]["x-real-ip"] == "${client4}", seen
          assert seen["uid"] != 0, seen
          assert seen["secret_sha256"] == hashlib.sha256(secret.encode()).hexdigest(), seen
          # Only its media port can be bound, and private/loopback are unreachable.
          assert seen["probes"] == {
              "bind udp media": "ok",
              "bind udp other": "Operation not permitted",
              "bind tcp media": "Operation not permitted",
              "send private": "Operation not permitted",
              "send loopback": "Operation not permitted",
          }, seen["probes"]
          assert 'allowed_origins = ["http://stream.test"]' in seen["config"], seen["config"]
          assert "secret_credential = \"turn-rest-secret\"" in seen["config"], seen["config"]
          assert secret not in seen["config"]
          assert head("/sfu", "-X", "POST").startswith("http/1.1 403")

          upgrade = client.succeed("""python3 - <<'EOF'
      import socket
      s = socket.create_connection(("${turn4}", 80), timeout=5)
      s.sendall(b"GET /sfu HTTP/1.1\\r\\nHost: stream.test\\r\\nUpgrade: websocket\\r\\nConnection: Upgrade\\r\\nOrigin: http://stream.test\\r\\nSec-WebSocket-Version: 13\\r\\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\\r\\n\\r\\n")
      data = b""
      while b"\\r\\n\\r\\n" not in data:
          chunk = s.recv(4096)
          if not chunk:
              break
          data += chunk
      print(data.split(b"\\r\\n\\r\\n")[0].decode())
      EOF""")
          print(upgrade)
          assert upgrade.startswith("HTTP/1.1 101"), upgrade
          assert "set-cookie" not in upgrade.lower(), upgrade

          turn.fail("grep -rqE '/sfu|/watch|/share' /var/log/nginx/")

      with subtest("SFU unit: dynamic user, nginx-only socket, restricted binds"):
          assert turn.succeed("systemctl show -p DynamicUser --value nuschl-sfu.service").strip() == "yes"
          assert turn.succeed("stat -c '%a %U %G' /run/nuschl-stream/sfu.sock").strip() == "660 root nginx"
          turn.fail("runuser -u nobody -- socat -u /dev/null UNIX-CONNECT:/run/nuschl-stream/sfu.sock")
          binds = turn.succeed("systemctl show -p SocketBindAllow -p SocketBindDeny -p IPAddressDeny nuschl-sfu.service")
          assert "40443" in binds and "SocketBindDeny=any" in binds and "127.0.0.0/8" in binds, binds

      with subtest("the runtime secret is in no file of the system's store closure"):
          closure = turn.succeed("nix-store -qR /run/current-system | tr '\\n' ' '")
          print(turn.succeed(f"du -sch {closure} | tail -1"))
          # Control: the search covers coturn's store configuration.
          turn.succeed(f"grep -rlF '#static-auth-secret#' {closure} | grep -q turnserver.conf")
          turn.fail(f"grep -rlF {secret} {closure}")
    '';
}
