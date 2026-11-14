# The web host under its NixOS module (docs/web.md): nginx serving the site, the
# share-link landing page and the app-link files, and the tunnel relay behind it.
# A client without a browser logs in to a stock Murmur through the relay (TLS on
# the client, as the web client will do in WebAssembly), and what the relay must
# refuse is refused. No ACME in the VM: plain HTTP.
{
  pkgs,
  relay,
  module,
}:
pkgs.testers.runNixOSTest {
  name = "nuschl-web";
  nodes = {
    web =
      { ... }:
      {
        imports = [ module ];
        services.nuschl-web = {
          enable = true;
          hostName = "web";
          enableACME = false;
          forceSSL = false;
          relay = {
            package = relay;
            # The test network is private; public servers need no allowance.
            allowNetworks = [ "192.168.1.0/24" ];
            settings.client_opens_per_minute = 20;
          };
          installUrl = "https://example.org/get-nuschl";
          appLinks = {
            android = [
              {
                package = "org.nuschl.android";
                certificateFingerprints = [ "AA:BB" ];
              }
            ];
            apple = [ "ABCDE12345.org.nuschl.Nuschl" ];
          };
        };
        networking.firewall.allowedTCPPorts = [ 80 ];
      };
    murmur =
      { ... }:
      {
        services.murmur = {
          enable = true;
          openFirewall = true;
          welcometext = "Welcome through the relay";
        };
        # Another TCP service on a port the relay would forward to: not Mumble.
        networking.firewall.allowedTCPPorts = [ 64740 ];
        systemd.services.not-mumble = {
          wantedBy = [ "multi-user.target" ];
          script = ''
            exec ${pkgs.socat}/bin/socat TCP-LISTEN:64740,fork,reuseaddr SYSTEM:'echo SSH-2.0-OpenSSH_9.9; sleep 5'
          '';
        };
      };
    client =
      { ... }:
      {
        environment.systemPackages = [
          relay
          pkgs.curl
          pkgs.jq
        ];
      };
  };
  testScript = ''
    start_all()
    web.wait_for_unit("nuschl-web-relay.service")
    web.wait_for_file("/run/nuschl-web-relay/relay.sock")
    web.wait_for_unit("nginx.service")
    web.wait_for_open_port(80)
    murmur.wait_for_unit("murmur.service")
    murmur.wait_for_open_port(64738)
    murmur.wait_for_open_port(64740)
    client.wait_for_unit("multi-user.target")

    # The site, the landing page for any token, and the app-link files.
    client.succeed("curl -sf http://web/ | grep -q 'example.org/get-nuschl'")
    page = client.succeed("curl -sf http://web/join/AAAAAAAAAAAAAAAAAAAAAA")
    assert "Join on Nuschl" in page and "/join.js" in page, page
    client.succeed("curl -sf http://web/join/ | grep -q 'Join on Nuschl'")
    client.succeed("curl -sf http://web/join.js | grep -q 'nuschl://join/'")
    headers = client.succeed("curl -sfI http://web/join/AAAAAAAAAAAAAAAAAAAAAA")
    assert "Content-Security-Policy" in headers and "no-referrer" in headers, headers
    assert "X-DNS-Prefetch-Control: off" in headers, headers
    client.succeed("curl -sf http://web/.well-known/assetlinks.json | jq -e '.[0].target.package_name == \"org.nuschl.android\"'")
    client.succeed("curl -sfI http://web/.well-known/assetlinks.json | grep -qi 'content-type: application/json'")
    client.succeed("curl -sf http://web/.well-known/apple-app-site-association | jq -e '.applinks.details[0].appIDs[0] == \"ABCDE12345.org.nuschl.Nuschl\"'")
    # Tokens in /join/ paths are not logged.
    web.fail("grep -r AAAAAAAAAAAAAAAAAAAAAA /var/log/nginx/")

    # A login to Murmur through the relay, TLS on the client: the relay pinged
    # the server, so datagrams go over UDP too.
    out = client.succeed("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server murmur:64738")
    assert "connected session=" in out and "udp=true udp_answered=true" in out, out
    # The relay never logs payloads; its journal names no user.
    web.fail("journalctl -u nuschl-web-relay | grep -q relay-probe")

    # What the relay refuses: other origins, other ports, local and private
    # addresses outside the allowance, and services that are not Mumble.
    client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://evil.example --server murmur:64738")
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server murmur:22")
    assert "PortRefused" in out, out
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server 127.0.0.1:64738")
    assert "AddressRefused" in out, out
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server 10.0.0.1:64738")
    assert "AddressRefused" in out, out
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server murmur:64740")
    assert "TLS" in out or "NotMumble" in out, out
    # The kernel refuses loopback to the relay as well (IPAddressDeny).
    web.succeed("systemctl show nuschl-web-relay -p IPAddressDeny | grep -q 127.0.0.0/8")
  '';
}
