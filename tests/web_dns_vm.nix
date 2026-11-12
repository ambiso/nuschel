# SPDX-License-Identifier: EUPL-1.2
# Two owned guests: actual resolved stub default and a dedicated custom resolver.
# All names are authoritative local answers; no upstream/public DNS or servers.
let
  source = builtins.getFlake ("path:" + toString ../.);
  pkgs = import source.inputs.nixpkgs { system = "x86_64-linux"; };
  lib = pkgs.lib;
  relay = source.packages.x86_64-linux.web-relay;
  module = import ../nix/web-module.nix { self = source; };
  node = resolved: { config, ... }: {
    imports = [ module ];
    virtualisation.memorySize = 768;
    # Keep the test LAN, but do not acquire a NAT/public default route or DNS.
    networking.useDHCP = false;
    networking.interfaces.eth0.useDHCP = lib.mkForce false;
    networking.nameservers = [ "127.0.0.55" ];
    networking.resolvconf.enable = false;
    system.nssDatabases.hosts = lib.mkForce [
      "files"
      "dns"
    ];
    # A Unix nscd/nsncd cache would bypass the service's IP filter even with
    # files+dns selected. Exercise DNS from the actual restricted process.
    services.nscd.enable = false;
    system.nssModules = lib.mkForce [ ];
    services.resolved = {
      enable = resolved;
      settings.Resolve = {
        DNS = "127.0.0.55";
        FallbackDNS = "";
        DNSSEC = "false";
        DNSOverTLS = "false";
        LLMNR = "false";
        MulticastDNS = "false";
      };
    };
    services.dnsmasq = {
      enable = true;
      resolveLocalQueries = false;
      settings = {
        listen-address = "127.0.0.55";
        bind-interfaces = true;
        no-resolv = true;
        no-hosts = true;
        log-queries = true;
        log-facility = "-";
        local = "/test/";
        address = [
          "/voice.dns.test/${config.networking.primaryIPAddress}"
          "/resolver.dns.test/${if resolved then "127.0.0.53" else "127.0.0.55"}"
          "/denied.dns.test/192.168.1.254"
        ];
      };
    };
    services.murmur = {
      enable = true;
      openFirewall = true;
      welcometext = "Owned DNS sandbox fixture";
    };
    services.nuschl-web = {
      enable = true;
      hostName = config.networking.hostName;
      enableACME = false;
      forceSSL = false;
      package = pkgs.emptyDirectory;
      relay = {
        package = relay;
        allowNetworks = [ "192.168.1.0/24" ];
        settings = {
          deny_networks = [ "192.168.1.254/32" ];
          client_opens_per_minute = 30;
        };
      }
      // lib.optionalAttrs (!resolved) {
        resolverAddresses = [ "127.0.0.55" ];
      };
    };
    systemd.services.nuschl-web-relay = {
      after = [
        "dnsmasq.service"
        "murmur.service"
      ]
      ++ lib.optional resolved "systemd-resolved.service";
      requires = [
        "dnsmasq.service"
        "murmur.service"
      ]
      ++ lib.optional resolved "systemd-resolved.service";
      environment.RES_OPTIONS = "attempts:1 timeout:1";
      serviceConfig.IPAccounting = true;
      serviceConfig.ExecStartPre = "${pkgs.python3}/bin/python ${./web_dns_guest.py} ${
        if resolved then "127.0.0.53" else "127.0.0.55"
      } ${config.networking.primaryIPAddress}";
    };
    environment.systemPackages = [
      relay
      pkgs.iproute2
      pkgs.python3
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "nuschl-web-dns";
  nodes = {
    resolved = node true;
    custom = node false;
  };
  # Actual driver state paths are checked BEFORE boot. Both environment paths
  # must point at the fresh owned disk tree used to invoke this driver.
  testScript = builtins.readFile ./vm_driver_scratch.py + "\n" + builtins.readFile ./web_dns_vm.py;
}
