# Evaluation only: no service execution, keys, ACME orders, DNS or network tests.
{
  projectRoot ? ../.,
  variant ? "supplied",
}:
let
  flake = builtins.getFlake (toString projectRoot);
  inherit (flake.inputs.nixpkgs) lib;
  system = lib.nixosSystem {
    system = "x86_64-linux";
    modules = [
      flake.nixosModules.default
      flake.nixosModules.web
      flake.nixosModules.witness
      ({ config, pkgs, ... }: {
        boot.isContainer = true;
        networking.useHostResolvConf = false;
        system.stateVersion = "26.11";
        # Stubs avoid constructing app builds. This tests module wiring, not
        # whether the supplied runtime verifier key is valid or a server works.
        services.nuschl = {
          enable = true;
          package = pkgs.writeShellScriptBin "nuschld" "exit 0";
          security = "compatible";
          hostName = "nuschl.eu";
          tls.useACMEHost = "nuschl.eu";
          superUserPasswordFile = "/run/agenix/nuschl-superuser-password";
          kt.origins = [ "nuschl.eu:64738" ];
          kt.witnesses = [
            {
              key = "fixture-only-not-a-runtime-verifier-key";
              url = "https://witness.nuschl.eu";
            }
          ];
          settings.share_links.web_host = "nuschl.eu";
        };
        services.nuschl-web = {
          enable = true;
          hostName = "nuschl.eu";
          package = pkgs.emptyDirectory;
          relay.package = pkgs.writeShellScriptBin "nuschl-web-relay" "exit 0";
        };
        services.nginx.virtualHosts."nuschl.eu".locations."= /".return = "302 /app/";
        services.nuschl-witness = {
          enable = true;
          package = pkgs.writeShellScriptBin "nuschl-witness" "exit 0";
          virtualHost = "witness.nuschl.eu";
          enableACME = true;
        };
        networking.firewall.interfaces = {
          ens3 = {
            allowedTCPPorts = [
              64738
            ]
            ++ lib.optionals (variant == "complete") [
              80
              443
            ];
            allowedUDPPorts = [ 64738 ];
          };
          tailscale0 = {
            allowedTCPPorts = [ 64738 ];
            allowedUDPPorts = [ 64738 ];
          };
        };
        # These are global requirements, not facts inferred from the user snippet.
        security.acme.acceptTerms = true;
        security.acme.defaults.email = "fixture@example.invalid";
        services.resolved.enable = variant == "resolved";
      })
    ];
  };
  cfg = system.config;
in
{
  failedAssertions = map (v: v.message) (builtins.filter (v: !v.assertion) cfg.assertions);
  server = {
    inherit (cfg.services.nuschl)
      hostName
      security
      port
      listenAddresses
      openFirewall
      ;
    database = cfg.services.nuschl.database;
    kt = cfg.services.nuschl.kt;
    exec = cfg.systemd.services.nuschl.serviceConfig.ExecStart;
    credentials = cfg.systemd.services.nuschl.serviceConfig.LoadCredential;
    stateDirectory = cfg.systemd.services.nuschl.serviceConfig.StateDirectory;
  };
  tls = {
    credentials = cfg.systemd.services.nuschl-tls-refresh.serviceConfig.LoadCredential;
    privateNetwork = cfg.systemd.services.nuschl-tls-refresh.serviceConfig.PrivateNetwork;
    timer = cfg.systemd.timers.nuschl-tls-refresh.timerConfig;
  };
  web = {
    inherit (cfg.services.nuschl-web) enableACME forceSSL;
    relay = cfg.services.nuschl-web.relay;
    redirect = cfg.services.nginx.virtualHosts."nuschl.eu".locations."= /".return;
    relayProxy = cfg.services.nginx.virtualHosts."nuschl.eu".locations."= /relay".proxyPass;
    kernelAllow = cfg.systemd.services.nuschl-web-relay.serviceConfig.IPAddressAllow;
    kernelDeny = cfg.systemd.services.nuschl-web-relay.serviceConfig.IPAddressDeny;
  };
  witness = {
    inherit (cfg.services.nuschl-witness)
      name
      openFirewall
      listenAddress
      port
      enableACME
      enrolPrivateAddresses
      keyFile
      ;
    stateDirectory = cfg.systemd.services.nuschl-witness.serviceConfig.StateDirectory;
  };
  firewall = {
    tcp = cfg.networking.firewall.allowedTCPPorts;
    udp = cfg.networking.firewall.allowedUDPPorts;
    ens3 = cfg.networking.firewall.interfaces.ens3;
    tailscale0 = cfg.networking.firewall.interfaces.tailscale0;
  };
  acme = builtins.mapAttrs (_: v: {
    inherit (v)
      domain
      directory
      webroot
      postRun
      ;
  }) cfg.security.acme.certs;
}
