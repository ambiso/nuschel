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
    lib.recursiveUpdate {
      inherit (cfg) security port;
      bind = cfg.listenAddresses;
      state_directory = stateDirectory;
      # Witnessing (docs/witness.md): where this server's epoch log is submitted.
      kt = {
        inherit (cfg.kt) origins;
        default_witnesses = cfg.kt.defaultWitnesses;
        witnesses = map (witness: { inherit (witness) key url; }) cfg.kt.witnesses;
      };
    } cfg.settings
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
    ]
    ++ lib.optionals (cfg.kt.signingKeyFile != null) [
      "--kt-signing-key-file"
      "%d/kt-signing-key"
    ]
    ++ lib.optionals (cfg.kt.vrfSecretFile != null) [
      "--kt-vrf-secret-file"
      "%d/kt-vrf-secret"
    ];
  notInStore = path: path == null || !(lib.hasPrefix builtins.storeDir (toString path));
  credentials =
    lib.optional (databaseUrlFile != null) "database-url:${databaseUrlFile}"
    ++ lib.optional (cfg.tls.certificateFile != null) "tls-certificate:${cfg.tls.certificateFile}"
    ++ lib.optional (cfg.tls.keyFile != null) "tls-key:${cfg.tls.keyFile}";
  # The sandbox of the server and of the one-time Murmur import.
  hardening = {
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
  # A Murmur database imported once before the first start (docs/server.md):
  # root copies it in for the dynamic user, the service imports it; the
  # marker keeps later starts from trying again.
  murmurCopy = "${stateDirectory}/murmur-import.sqlite";
  murmurMarker = "${stateDirectory}/murmur-imported";
  copyMurmur = pkgs.writeShellScript "nuschel-copy-murmur" ''
    set -eu
    if [ ! -e ${murmurMarker} ] && [ ! -e ${murmurCopy} ]; then
      ${pkgs.coreutils}/bin/install -m 0600 -o nuschel -g nuschel \
        ${lib.escapeShellArg (toString cfg.importMurmur.databaseFile)} ${murmurCopy}
    fi
  '';
  importMurmur = pkgs.writeShellScript "nuschel-import-murmur" ''
    set -eu
    if [ -e ${murmurCopy} ] && [ ! -e ${murmurMarker} ]; then
      # Takes the operator lock, as every admin command does.
      ${cfg.package}/bin/nuscheld --config ${configFile} --state-directory ${stateDirectory} "$@" \
        admin import-murmur ${murmurCopy} --murmur-server-id ${toString cfg.importMurmur.serverId} \
        ${lib.optionalString cfg.importMurmur.replaceExisting "--replace-existing"}
      ${pkgs.coreutils}/bin/touch ${murmurMarker}
      ${pkgs.coreutils}/bin/rm -f ${murmurCopy}
    fi
  '';
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
      # "e2ee" becomes the default once voice is end-to-end encrypted too (SFrame).
      default = "compatible";
      description = ''
        "compatible" admits stock Mumble clients and has no end-to-end encryption.
        "e2ee" turns stock clients away, carries text only in MLS groups (every
        channel and direct message) and admits unregistered users only to register;
        voice is not end-to-end encrypted yet (docs/server.md, "Security mode").
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
    kt = {
      signingKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Runtime file with the key-transparency head-signing key, the server's
          identity key that clients pin (32 random bytes or 64 hex digits; LoadCredential).
          Without it, nuscheld generates one once in its state directory.
        '';
      };
      vrfSecretFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Runtime file with the key-transparency VRF secret (same format; LoadCredential).
          It can never change for the life of the log. Without it, nuscheld generates
          one once in its state directory.
        '';
      };
      origins = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = lib.optional (config.networking.domain != null) (
          lib.toLower "${config.networking.hostName}.${config.networking.domain}:${toString cfg.port}"
        );
        defaultText = lib.literalExpression ''[ "''${networking.fqdn}:''${port}" ]'';
        example = [ "chat.example.org:64738" ];
        description = ''
          The names clients reach this server under, `host:port` in lower case as in
          their bookmarks. Each is an origin of the server's epoch log whose
          checkpoints the witnesses cosign (docs/witness.md); clients that require
          witnesses accept only the origin they connect to.
        '';
      };
      defaultWitnesses = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Submit to Nuschel's built-in witnesses (the project auditor; none until it
          is deployed).
        '';
      };
      witnesses = lib.mkOption {
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              key = lib.mkOption {
                type = lib.types.str;
                description = "The witness's verifier key (`nuschel-witness vkey`).";
              };
              url = lib.mkOption {
                type = lib.types.str;
                example = "https://witness.example.org";
                description = "Its base URL.";
              };
            };
          }
        );
        default = [ ];
        description = "More witnesses to submit this server's epoch log to.";
      };
    };
    importMurmur = {
      databaseFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "/var/lib/murmur/murmur.sqlite";
        description = ''
          A Murmur (mumble-server) SQLite database to import once, before the first
          start (`nuscheld --import-murmur`, docs/server.md): users with their
          passwords, channels, ACLs, groups, links, listeners and bans. Stop Murmur
          first. The import refuses a database that already has channels, users or
          bans (see replaceExisting); then nuschel-import-murmur.service fails,
          nuschel.service does not start, and the journal says why. It runs as its
          own unit before the server, without a start timeout.
          Settings found in it are written to murmur-import.toml in the state
          directory, for `settings`.
        '';
      };
      serverId = lib.mkOption {
        type = lib.types.int;
        default = 1;
        description = "The Murmur virtual server to import.";
      };
      replaceExisting = lib.mkEnableOption ''
        replacing this server's channels, registered users and bans with the import
        (`--replace-existing`); without it, a server with state refuses the import
      '';
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
          cfg.kt.signingKeyFile
          cfg.kt.vrfSecretFile
        ];
        message = "Nuschel secrets (tls.keyFile, database.urlFile, passwordFile, superUserPasswordFile, kt.signingKeyFile, kt.vrfSecretFile) must not be in the world-readable Nix store.";
      }
      {
        assertion =
          !(cfg.settings ? kt && (cfg.settings.kt ? signing_key_file || cfg.settings.kt ? vrf_secret_file));
        message = "Use services.nuschel.kt.signingKeyFile / vrfSecretFile instead of key paths in settings.";
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
          ) "superuser-password:${cfg.superUserPasswordFile}"
          ++ lib.optional (cfg.kt.signingKeyFile != null) "kt-signing-key:${cfg.kt.signingKeyFile}"
          ++ lib.optional (cfg.kt.vrfSecretFile != null) "kt-vrf-secret:${cfg.kt.vrfSecretFile}";
        # Migrations and a connection check before the server takes traffic,
        # under the operator lock: never while `nuscheld admin` (or an import)
        # uses the database.
        ExecStartPre = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/nuschel-storage-worker"
            "--server"
            "--check-database"
            "--operator-lock"
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
      }
      // hardening;
    };
    # The one-time Murmur import, before the server's first start (docs/server.md).
    systemd.services.nuschel-import-murmur = lib.mkIf (cfg.importMurmur.databaseFile != null) {
      description = "Import a Murmur database into the Nuschel server, once";
      requiredBy = [ "nuschel.service" ];
      before = [ "nuschel.service" ];
      after = [ "network.target" ] ++ lib.optional localPostgres "postgresql.target";
      requires = lib.optional localPostgres "postgresql.target";
      unitConfig.ConditionPathExists = "!${murmurMarker}";
      serviceConfig = {
        Type = "oneshot";
        DynamicUser = true;
        User = "nuschel";
        StateDirectory = "nuschel";
        StateDirectoryMode = "0700";
        WorkingDirectory = stateDirectory;
        UMask = "0077";
        LoadCredential = credentials;
        # Reading and writing a large server can take a while.
        TimeoutStartSec = "infinity";
        ExecStartPre = "+${copyMurmur}";
        ExecStart = lib.escapeShellArgs ([ "${importMurmur}" ] ++ serverDatabaseArguments ++ tlsArguments);
      }
      // hardening
      // {
        # The reader confines itself with Landlock (@sandbox).
        SystemCallFilter = [
          "@system-service"
          "@sandbox"
          "~@privileged"
        ];
      };
    };
  };
}
