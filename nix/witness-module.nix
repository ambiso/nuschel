{ self }:
# The project auditor for Nuschel's key transparency (docs/witness.md):
# `services.nuschel-witness`. Defaults give a working auditor; with a host name
# and ACME it is served over HTTPS by nginx:
#
#   services.nuschel-witness = {
#     enable = true;
#     virtualHost = "witness.example.org";
#     enableACME = true;
#     openFirewall = true;
#   };
#   security.acme = { acceptTerms = true; defaults.email = "admin@example.org"; };
#
# Its verifier key (what clients and servers configure) is in the journal and at
# https://witness.example.org/nuschel-kt/v1/vkey.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschel-witness;
  stateDirectory = "/var/lib/nuschel-witness";
  notInStore = path: path == null || !(lib.hasPrefix builtins.storeDir (toString path));
  arguments = [
    "${cfg.package}/bin/nuschel-witness"
    "serve"
    "--name"
    cfg.name
    "--state-directory"
    stateDirectory
    "--listen"
    "${cfg.listenAddress}:${toString cfg.port}"
    "--accept-new-logs"
    (lib.boolToString cfg.acceptNewLogs)
    "--max-logs"
    (toString cfg.maxLogs)
    "--submissions-per-minute"
    (toString cfg.submissionsPerMinute)
    "--enrolments-per-minute"
    (toString cfg.enrolmentsPerMinute)
    "--max-request-bytes"
    (toString cfg.maxRequestBytes)
  ]
  ++ lib.optionals (cfg.keyFile != null) [
    "--key-file"
    "%d/witness-key"
  ]
  ++ lib.optional cfg.enrolPrivateAddresses "--enrol-private-addresses";
in
{
  options.services.nuschel-witness = {
    enable = lib.mkEnableOption "nuschel-witness, the auditor that checks and cosigns Nuschel servers' key-transparency logs";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.witness;
      defaultText = lib.literalExpression "inputs.nuschel.packages.\${pkgs.stdenv.hostPlatform.system}.witness";
      description = "Package containing nuschel-witness.";
    };
    name = lib.mkOption {
      type = lib.types.str;
      default =
        if cfg.virtualHost != null then
          "${cfg.virtualHost}/nuschel-kt"
        else
          "${config.networking.hostName}/nuschel-kt";
      defaultText = lib.literalExpression ''"''${virtualHost}/nuschel-kt"'';
      description = ''
        The cosigner name clients configure with the key (C2SP: at most 255 bytes,
        no spaces or "+"). Changing it changes the verifier key.
      '';
    };
    keyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Runtime file with the ML-DSA-44 seed (32 random bytes or 64 hex digits;
        LoadCredential). Without it, one is generated once in ${stateDirectory}.
        Back it up: clients trust this key, and a new one means a new witness.
      '';
    };
    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Address of the plain-HTTP listener (nginx proxies to it when virtualHost is set).";
    };
    port = lib.mkOption {
      type = lib.types.port;
      default = 8473;
    };
    virtualHost = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "witness.example.org";
      description = "Serve the witness through nginx under this host name; servers submit to https://<virtualHost>.";
    };
    enableACME = lib.mkEnableOption "an ACME certificate for virtualHost (HTTPS; set security.acme.acceptTerms and an email)";
    openFirewall = lib.mkEnableOption "opening 80 and 443 (with virtualHost) or the listener's port";
    acceptNewLogs = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Enrol servers it has not seen before (after checking their hello at host:port).";
    };
    maxLogs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 10000;
    };
    submissionsPerMinute = lib.mkOption {
      type = lib.types.ints.positive;
      default = 60;
      description = "Per server origin.";
    };
    enrolmentsPerMinute = lib.mkOption {
      type = lib.types.ints.positive;
      default = 10;
    };
    maxRequestBytes = lib.mkOption {
      type = lib.types.ints.positive;
      default = 16 * 1024 * 1024;
      description = "Largest submission (a large server's first epoch carries its whole directory).";
    };
    enrolPrivateAddresses = lib.mkEnableOption ''
      enrolling servers on loopback, private and link-local addresses (private
      deployments and tests; a public witness should not connect there)
    '';
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = notInStore cfg.keyFile;
        message = "services.nuschel-witness.keyFile must not be in the world-readable Nix store.";
      }
      {
        assertion = !cfg.enableACME || cfg.virtualHost != null;
        message = "services.nuschel-witness.enableACME needs virtualHost.";
      }
    ];
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall (
      if cfg.virtualHost != null then
        [
          80
          443
        ]
      else
        [ cfg.port ]
    );
    services.nginx = lib.mkIf (cfg.virtualHost != null) {
      enable = true;
      recommendedProxySettings = true;
      virtualHosts.${cfg.virtualHost} = {
        enableACME = cfg.enableACME;
        forceSSL = cfg.enableACME;
        locations."/" = {
          proxyPass = "http://${cfg.listenAddress}:${toString cfg.port}";
          extraConfig = ''
            client_max_body_size ${toString cfg.maxRequestBytes};
            proxy_request_buffering on;
          '';
        };
      };
    };
    systemd.services.nuschel-witness = {
      description = "Nuschel key-transparency witness";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        User = "nuschel-witness";
        StateDirectory = "nuschel-witness";
        StateDirectoryMode = "0700";
        WorkingDirectory = stateDirectory;
        UMask = "0077";
        LoadCredential = lib.optional (cfg.keyFile != null) "witness-key:${cfg.keyFile}";
        ExecStart = lib.escapeShellArgs arguments;
        Restart = "on-failure";
        RestartSec = "5s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        PrivateIPC = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        # Outbound TCP for enrolment checks; DNS through the system resolver.
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        SocketBindAllow = [ "tcp:${toString cfg.port}" ];
        SocketBindDeny = "any";
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
      };
    };
  };
}
