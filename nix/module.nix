{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschel;
  toml = pkgs.formats.toml { };
  stateDirectory = "/var/lib/nuschel";
  localPostgres = cfg.database.type == "postgresql" && cfg.database.createLocally;
  databaseUrlFile =
    if localPostgres then
      pkgs.writeText "nuschel-local-postgresql-url" "postgresql:///nuschel?host=/run/postgresql&user=nuschel\n"
    else
      cfg.database.urlFile;
  # Secrets never go into this file: it is in the world-readable Nix store.
  configFile = toml.generate "nuscheld.toml" (
    {
      inherit (cfg) security port;
      bind = cfg.listenAddresses;
      state_directory = stateDirectory;
    }
    // cfg.settings
  );
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
  secretArguments =
    lib.optionals (cfg.passwordFile != null) [
      "--password-file"
      "%d/server-password"
    ]
    ++ lib.optionals (cfg.superUserPasswordFile != null) [
      "--superuser-password-file"
      "%d/superuser-password"
    ];
  notInStore = path: path == null || !(lib.hasPrefix builtins.storeDir (toString path));
in
{
  options.services.nuschel = {
    enable = lib.mkEnableOption "the Nuschel server (Mumble-compatible; phase 1)";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.server;
      defaultText = lib.literalExpression "inputs.nuschel.packages.\${pkgs.stdenv.hostPlatform.system}.server";
      description = "Package containing nuscheld and nuschel-storage-worker.";
    };
    security = lib.mkOption {
      type = lib.types.enum [
        "compatible"
        "e2ee"
      ];
      # "e2ee" becomes the default once end-to-end encryption is implemented;
      # until then nuscheld refuses to start with it.
      default = "compatible";
      description = ''
        "compatible" admits stock Mumble clients and has no end-to-end encryption.
        "e2ee" is not implemented yet; the server refuses to start with it.
      '';
    };
    listenAddresses = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "0.0.0.0"
        "::"
      ];
      description = "Addresses for TCP and UDP; IPv6 sockets are IPv6-only.";
    };
    port = lib.mkOption {
      type = lib.types.port;
      default = 64738;
    };
    openFirewall = lib.mkEnableOption "opening the server's TCP and UDP ports";
    settings = lib.mkOption {
      type = toml.type;
      default = { };
      example = lib.literalExpression ''
        {
          welcome_text = "Welcome!";
          max_users = 100;
          autoban.attempts = 10;
        }
      '';
      description = ''
        Further nuscheld.toml settings (see apps/server/nuscheld.example.toml).
        The generated file is in the Nix store: never put secrets here; use
        passwordFile and superUserPasswordFile.
      '';
    };
    passwordFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Runtime file with the server password for unregistered users (LoadCredential).";
    };
    superUserPasswordFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Runtime file with the SuperUser password, applied at every start
        (LoadCredential). Without one, SuperUser cannot log in.
      '';
    };
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
        default = "${stateDirectory}/server.sqlite3";
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
        description = "PEM certificate chain. Without one, a self-signed certificate is generated once and kept.";
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
      {
        assertion = lib.all notInStore [
          cfg.tls.keyFile
          cfg.database.urlFile
          cfg.passwordFile
          cfg.superUserPasswordFile
        ];
        message = "Nuschel secrets (tls.keyFile, database.urlFile, passwordFile, superUserPasswordFile) must not be in the world-readable Nix store.";
      }
      {
        assertion =
          !(
            cfg.settings ? password || cfg.settings ? password_file || cfg.settings ? superuser_password_file
          );
        message = "Use services.nuschel.passwordFile / superUserPasswordFile instead of secrets in settings.";
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
      after = [ "network.target" ] ++ lib.optional localPostgres "postgresql.target";
      requires = lib.optional localPostgres "postgresql.target";
      restartTriggers = [ configFile ];
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        User = "nuschel";
        StateDirectory = "nuschel";
        StateDirectoryMode = "0700";
        WorkingDirectory = stateDirectory;
        UMask = "0077";
        LoadCredential =
          lib.optional (databaseUrlFile != null) "database-url:${databaseUrlFile}"
          ++ lib.optional (cfg.tls.certificateFile != null) "tls-certificate:${cfg.tls.certificateFile}"
          ++ lib.optional (cfg.tls.keyFile != null) "tls-key:${cfg.tls.keyFile}"
          ++ lib.optional (cfg.passwordFile != null) "server-password:${cfg.passwordFile}"
          ++ lib.optional (
            cfg.superUserPasswordFile != null
          ) "superuser-password:${cfg.superUserPasswordFile}";
        # Migrations and a connection check before the server takes traffic.
        ExecStartPre = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/nuschel-storage-worker"
            "--server"
            "--check-database"
          ]
          ++ databaseArguments
        );
        ExecStart = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/nuscheld"
            "--config"
            configFile
            "--state-directory"
            stateDirectory
          ]
          ++ serverDatabaseArguments
          ++ tlsArguments
          ++ secretArguments
        );
        Restart = "on-failure";
        RestartSec = "5s";
        LimitNOFILE = 65536;
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
        SocketBindAllow = [
          "tcp:${toString cfg.port}"
          "udp:${toString cfg.port}"
        ];
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
