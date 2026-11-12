{
  projectRoot ? ../.,
  variant ? "http01",
}:
let
  flake = builtins.getFlake (toString projectRoot);
  inherit (flake.inputs.nixpkgs) lib;
  cohost =
    !(builtins.elem variant [
      "dns"
      "disabled"
    ]);
  evaluated = lib.nixosSystem {
    system = "x86_64-linux";
    modules = [
      flake.nixosModules.default
      flake.nixosModules.web
      flake.nixosModules.witness
      ({ pkgs, ... }: {
        boot.isContainer = true;
        networking.useHostResolvConf = false;
        networking.enableIPv6 = variant != "ipv4";
        system.stateVersion = "26.11";
        services.nuschl = {
          enable = true;
          package = pkgs.writeShellScriptBin "nuschld" "exit 0";
          hostName = "voice.example.test";
          tls = {
            enableACME = variant != "no-acme";
            nginxHTTP01 = variant != "disabled";
          };
        };
        services.nuschl-web = {
          enable = cohost;
          hostName = "web.example.test";
          package = pkgs.emptyDirectory;
          relay.package = pkgs.writeShellScriptBin "nuschl-web-relay" "exit 0";
        };
        services.nuschl-witness = {
          enable = cohost;
          virtualHost = "witness.example.test";
          enableACME = true;
          package = pkgs.writeShellScriptBin "nuschl-witness" "exit 0";
        };
        security.acme = {
          acceptTerms = true;
          defaults = {
            email = "fixture@example.invalid";
          }
          //
            lib.optionalAttrs
              (builtins.elem variant [
                "dns"
                "disabled"
              ])
              {
                dnsProvider = "fixture-provider";
              };
          certs =
            lib.optionalAttrs
              (builtins.elem variant [
                "dns"
                "disabled"
              ])
              {
                "other.example.test" = { };
              }
            // lib.optionalAttrs (variant == "group-conflict") {
              "voice.example.test".group = lib.mkForce "nginx";
            };
        };
        services.nginx.virtualHosts = lib.optionalAttrs (variant == "ssl-conflict") {
          "voice.example.test" = {
            forceSSL = true;
            sslCertificate = "/fixture/voice-cert.pem";
            sslCertificateKey = "/fixture/voice-key.pem";
          };
        };
      })
    ];
  };
  cfg = evaluated.config;
  voice = "voice.example.test";
in
{
  failedAssertions = map (v: v.message) (builtins.filter (v: !v.assertion) cfg.assertions);
  nginx = {
    inherit (cfg.services.nginx) enable;
    virtualHosts = builtins.mapAttrs (
      _: v:
      let
        certificate = builtins.tryEval v.sslCertificate;
        key = builtins.tryEval v.sslCertificateKey;
      in
      {
        inherit (v)
          listen
          enableACME
          useACMEHost
          addSSL
          forceSSL
          onlySSL
          rejectSSL
          locations
          ;
        sslCertificate = if certificate.success then certificate.value else null;
        sslCertificateKey = if key.success then key.value else null;
      }
    ) cfg.services.nginx.virtualHosts;
    inaccessible = cfg.systemd.services.nginx.serviceConfig.InaccessiblePaths or [ ];
    groups = cfg.users.users.nginx.extraGroups or [ ];
    after = cfg.systemd.services.nginx.after or [ ];
  };
  acme = builtins.mapAttrs (_: v: {
    inherit (v)
      domain
      group
      webroot
      dnsProvider
      listenHTTP
      s3Bucket
      postRun
      ;
  }) cfg.security.acme.certs;
  certificateUnit = {
    inherit (cfg.systemd.services."acme-${voice}") after wants;
  };
  server = {
    inherit (cfg.systemd.services.nuschl.serviceConfig) SupplementaryGroups InaccessiblePaths;
    exec = cfg.systemd.services.nuschl.serviceConfig.ExecStart;
  };
  publisher = cfg.systemd.services.nuschl-tls-refresh.serviceConfig.LoadCredential;
  firewallTCP = cfg.networking.firewall.allowedTCPPorts;
}
