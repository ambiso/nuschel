{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschel;
  localPostgres = cfg.database.type == "postgresql" && cfg.database.createLocally;
  databaseUrlFile =
    if localPostgres then
      pkgs.writeText "nuschel-local-postgresql-url" "postgresql:///nuschel?host=/run/postgresql&user=nuschel\n"
    else
      cfg.database.urlFile;
  databaseArguments =
    if cfg.database.type == "sqlite" then
      [
        "--database"
        cfg.database.sqlitePath
      ]
    else
      [
        "--database-url-file"
        "%d/database-url"
      ];
  serverDatabaseArguments =
    if cfg.database.type == "sqlite" then
      [
        "--sqlite-path"
        cfg.database.sqlitePath
      ]
    else
      [
        "--database-url-file"
        "%d/database-url"
      ];
  tlsArguments = lib.optionals (cfg.tls.certificateFile != null && cfg.tls.keyFile != null) [
    "--tls-certificate-file"
    "%d/tls-certificate"
    "--tls-key-file"
    "%d/tls-key"
  ];
in
{
  options.services.nuschel = {
    enable = lib.mkEnableOption "the Nuschel server (network sessions are still under development)";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.server;
      defaultText = lib.literalExpression "inputs.nuschel.packages.\${pkgs.stdenv.hostPlatform.system}.server";
      description = "Server and storage worker package.";
    };
    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0";
    };
    port = lib.mkOption {
      type = lib.types.port;
      default = 64738;
    };
    openFirewall = lib.mkEnableOption "opening the server's TCP and UDP ports";
    database = {
      type = lib.mkOption {
        type = lib.types.enum [
          "sqlite"
          "postgresql"
        ];
        default = "sqlite";
      };
      sqlitePath = lib.mkOption {
        type = lib.types.str;
        default = "/var/lib/nuschel/server.sqlite3";
      };
      urlFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Runtime credential file containing a PostgreSQL URL; do not put a password in a Nix string.";
      };
      createLocally = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Provision PostgreSQL with Unix-socket peer authentication for nuschel.";
      };
    };
    tls = {
      certificateFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
      };
      keyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Runtime private-key path, imported with systemd LoadCredential.";
      };
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.database.type != "postgresql" || cfg.database.createLocally || cfg.database.urlFile != null;
        message = "Nuschel PostgreSQL requires database.urlFile or database.createLocally.";
      }
      {
        assertion = !cfg.database.createLocally || cfg.database.type == "postgresql";
        message = "Nuschel database.createLocally requires PostgreSQL.";
      }
      {
        assertion = !cfg.database.createLocally || cfg.database.urlFile == null;
        message = "Choose local peer authentication or database.urlFile, not both.";
      }
      {
        assertion = (cfg.tls.certificateFile == null) == (cfg.tls.keyFile == null);
        message = "Nuschel TLS requires both certificateFile and keyFile.";
      }
    ];
    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [ cfg.port ];
      allowedUDPPorts = [ cfg.port ];
    };
    services.postgresql = lib.mkIf localPostgres {
      enable = true;
      ensureDatabases = [ "nuschel" ];
      ensureUsers = [
        {
          name = "nuschel";
          ensureDBOwnership = true;
        }
      ];
    };
    systemd.services.nuschel = {
      description = "Nuschel Mumble-compatible server";
      wantedBy = [ "multi-user.target" ];
      # The service can listen before postgresql-setup creates roles/databases.
      after = [ "network.target" ] ++ lib.optional localPostgres "postgresql.target";
      requires = lib.optional localPostgres "postgresql.target";
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        User = "nuschel";
        StateDirectory = "nuschel";
        StateDirectoryMode = "0700";
        WorkingDirectory = "/var/lib/nuschel";
        UMask = "0077";
        LoadCredential =
          lib.optional (databaseUrlFile != null) "database-url:${databaseUrlFile}"
          ++ lib.optional (cfg.tls.certificateFile != null) "tls-certificate:${cfg.tls.certificateFile}"
          ++ lib.optional (cfg.tls.keyFile != null) "tls-key:${cfg.tls.keyFile}";
        ExecStartPre = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/nuschel-storage-worker"
            "--check-database"
          ]
          ++ databaseArguments
        );
        ExecStart = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/nuscheld"
            "--listen-address"
            cfg.listenAddress
            "--port"
            (toString cfg.port)
          ]
          ++ serverDatabaseArguments
          ++ tlsArguments
        );
        Restart = "on-failure";
        RestartSec = "5s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        SystemCallFilter = [ "@system-service" ];
      };
    };
  };
}
