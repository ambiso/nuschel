{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschl;
  toml = pkgs.formats.toml { };
  stateDirectory = "/var/lib/nuschl";
  acmeHost = if cfg.tls.enableACME then cfg.hostName else cfg.tls.useACMEHost;
  useACME = acmeHost != null;
  tlsHost =
    if cfg.hostName != null then
      cfg.hostName
    else if useACME then
      config.security.acme.certs.${acmeHost}.domain
    else
      null;
  acmeDirectory = if useACME then config.security.acme.certs.${acmeHost}.directory else "";
  tlsRuntime = "/run/nuschl-tls";
  useHTTP01 = cfg.tls.nginxHTTP01 && cfg.tls.enableACME && cfg.hostName != null;
  http01Host = if cfg.hostName == null then "invalid-nuschl-http01.invalid" else cfg.hostName;
  http01Webroot = "/var/lib/nuschl-acme-challenge";
  http01Vhost = config.services.nginx.virtualHosts.${http01Host};
  localPostgres = cfg.database.type == "postgresql" && cfg.database.createLocally;
  databaseUrlFile =
    if localPostgres then
      pkgs.writeText "nuschl-local-postgresql-url" "postgresql:///nuschl?host=/run/postgresql&user=nuschl\n"
    else
      cfg.database.urlFile;
  # Secrets never go into this file: it is in the world-readable Nix store.
  configFile = toml.generate "nuschld.toml" (
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
  tlsArguments =
    if useACME then
      [
        "--tls-certificate-file"
        "${tlsRuntime}/current/fullchain.pem"
        "--tls-key-file"
        "${tlsRuntime}/current/key.pem"
      ]
    else
      lib.optionals (cfg.tls.certificateFile != null && cfg.tls.keyFile != null) [
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
  # PID 1 reads only the selected ACME pair into this helper's credentials.
  # The server never joins the broad ACME group or sees that directory.
  refreshTLS = pkgs.writeShellScript "nuschl-refresh-tls" ''
    set -euo pipefail
    tlsCurrent=$(${pkgs.coreutils}/bin/readlink "$RUNTIME_DIRECTORY/current" || true)
    # Recover safely from a killed helper without accumulating staged keys.
    for tlsEntry in "$RUNTIME_DIRECTORY"/.staging.* "$RUNTIME_DIRECTORY"/generation.*; do
      [ -d "$tlsEntry" ] || continue
      [ "$tlsEntry" = "$tlsCurrent" ] || ${pkgs.coreutils}/bin/rm -rf -- "$tlsEntry"
    done
    tlsStage=$(${pkgs.coreutils}/bin/mktemp -d "$RUNTIME_DIRECTORY/.staging.XXXXXXXXXX")
    trap '${pkgs.coreutils}/bin/rm -rf -- "$tlsStage"' EXIT
    ${pkgs.coreutils}/bin/install -m 0440 "$CREDENTIALS_DIRECTORY/fullchain" "$tlsStage/fullchain.pem"
    ${pkgs.coreutils}/bin/install -m 0440 "$CREDENTIALS_DIRECTORY/key" "$tlsStage/key.pem"
    if [ "$(${pkgs.coreutils}/bin/stat -c %s "$tlsStage/fullchain.pem")" -gt 262144 ] \
      || [ "$(${pkgs.coreutils}/bin/stat -c %s "$tlsStage/key.pem")" -gt 65536 ]; then
      echo "Refusing oversized TLS credentials; previous certificate retained." >&2
      exit 1
    fi
    # No private bytes or decoder diagnostics are printed to the journal.
    if ! ${pkgs.openssl}/bin/openssl verify -purpose sslserver \
      -verify_hostname ${lib.escapeShellArg (if tlsHost == null then "" else tlsHost)} \
      -CAfile ${config.security.pki.caBundle} \
      -untrusted "$tlsStage/fullchain.pem" "$tlsStage/fullchain.pem" >/dev/null 2>&1; then
      echo "Refusing an untrusted, expired or wrong-hostname TLS chain; previous certificate retained." >&2
      exit 1
    fi
    if ! ${pkgs.openssl}/bin/openssl x509 -in "$tlsStage/fullchain.pem" -pubkey -noout 2>/dev/null \
      | ${pkgs.openssl}/bin/openssl pkey -pubin -outform DER -out "$tlsStage/certificate-public.der" 2>/dev/null \
      || ! ${pkgs.openssl}/bin/openssl pkey -in "$tlsStage/key.pem" -pubout -outform DER -out "$tlsStage/key-public.der" 2>/dev/null \
      || ! ${pkgs.diffutils}/bin/cmp -s "$tlsStage/certificate-public.der" "$tlsStage/key-public.der"; then
      echo "Refusing an invalid or mismatching TLS key; previous certificate retained." >&2
      exit 1
    fi
    ${pkgs.coreutils}/bin/rm -- "$tlsStage/certificate-public.der" "$tlsStage/key-public.der"
    ${pkgs.coreutils}/bin/chmod 0750 "$tlsStage"
    tlsOld=$(${pkgs.coreutils}/bin/readlink "$RUNTIME_DIRECTORY/current" || true)
    tlsGeneration="$RUNTIME_DIRECTORY/generation.''${tlsStage##*.staging.}"
    ${pkgs.coreutils}/bin/mv -T -- "$tlsStage" "$tlsGeneration"
    ${pkgs.coreutils}/bin/rm -f -- "$RUNTIME_DIRECTORY/.next"
    ${pkgs.coreutils}/bin/ln -s -- "$tlsGeneration" "$RUNTIME_DIRECTORY/.next"
    ${pkgs.coreutils}/bin/mv -Tf -- "$RUNTIME_DIRECTORY/.next" "$RUNTIME_DIRECTORY/current"
    # Only names previously published by this helper can be removed.
    tlsOldName="''${tlsOld##*/}"
    if [ "$tlsOld" = "$RUNTIME_DIRECTORY/$tlsOldName" ]; then
      case "$tlsOldName" in generation.*)
        [ "$tlsOld" = "$tlsGeneration" ] || ${pkgs.coreutils}/bin/rm -rf -- "$tlsOld"
        ;;
      esac
    fi
  '';
  signalTLS = pkgs.writeShellScript "nuschl-signal-tls" ''
    set -eu
    if ${pkgs.systemd}/bin/systemctl is-active --quiet nuschl.service; then
      ${pkgs.systemd}/bin/systemctl kill --kill-whom=main --signal=HUP nuschl.service
    fi
  '';
  requireTLS = pkgs.writeShellScript "nuschl-require-published-tls" ''
    set -eu
    if [ ! -r "$1/current/fullchain.pem" ] || [ ! -r "$1/current/key.pem" ]; then
      echo "Waiting for a validated Nuschl TLS certificate; startup will retry." >&2
      exit 1
    fi
  '';
  # A Murmur database imported once before the first start (docs/server.md):
  # root copies it in for the dynamic user, the service imports it; the
  # marker keeps later starts from trying again.
  murmurCopy = "${stateDirectory}/murmur-import.sqlite";
  murmurMarker = "${stateDirectory}/murmur-imported";
  copyMurmur = pkgs.writeShellScript "nuschl-copy-murmur" ''
    set -eu
    if [ ! -e ${murmurMarker} ] && [ ! -e ${murmurCopy} ]; then
      ${pkgs.coreutils}/bin/install -m 0600 -o nuschl -g nuschl \
        ${lib.escapeShellArg (toString cfg.importMurmur.databaseFile)} ${murmurCopy}
    fi
  '';
  importMurmur = pkgs.writeShellScript "nuschl-import-murmur" ''
    set -eu
    if [ -e ${murmurCopy} ] && [ ! -e ${murmurMarker} ]; then
      # Takes the operator lock, as every admin command does.
      ${cfg.package}/bin/nuschld --config ${configFile} --state-directory ${stateDirectory} "$@" \
        admin import-murmur ${murmurCopy} --murmur-server-id ${toString cfg.importMurmur.serverId} \
        ${lib.optionalString cfg.importMurmur.replaceExisting "--replace-existing"}
      ${pkgs.coreutils}/bin/touch ${murmurMarker}
      ${pkgs.coreutils}/bin/rm -f ${murmurCopy}
    fi
  '';
in
{
  imports = [ (lib.mkRenamedOptionModule [ "services" "nuschel" ] [ "services" "nuschl" ]) ];
  options.services.nuschl = {
    enable = lib.mkEnableOption "the Nuschl server (Mumble-compatible; phase 1)";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.server;
      defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.server";
      description = "Package containing nuschld and nuschl-storage-worker.";
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
        voice is disabled until end-to-end media exists (docs/server.md, "Security mode").
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
    hostName = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "voice.example.org";
      description = "Public DNS hostname clients connect to; required for tls.enableACME and used to validate the selected certificate.";
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
        Further nuschld.toml settings (see apps/server/nuschld.example.toml).
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
        description = "Provision PostgreSQL with Unix-socket peer authentication for nuschl.";
      };
    };
    kt = {
      signingKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Runtime file with the key-transparency head-signing key, the server's
          identity key that clients pin (32 random bytes or 64 hex digits; LoadCredential).
          Without it, nuschld generates one once in its state directory.
        '';
      };
      vrfSecretFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Runtime file with the key-transparency VRF secret (same format; LoadCredential).
          It can never change for the life of the log. Without it, nuschld generates
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
          Submit to Nuschl's built-in witnesses (the project auditor; none until it
          is deployed).
        '';
      };
      witnesses = lib.mkOption {
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              key = lib.mkOption {
                type = lib.types.str;
                description = "The witness's verifier key (`nuschl-witness vkey`).";
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
          start (`nuschld --import-murmur`, docs/server.md): users with their
          passwords, channels, ACLs, groups, links, listeners and bans. Stop Murmur
          first. The import refuses a database that already has channels, users or
          bans (see replaceExisting); then nuschl-import-murmur.service fails,
          nuschl.service does not start, and the journal says why. It runs as its
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
      enableACME = lib.mkEnableOption "an ACME certificate for hostName (configure security.acme's challenge and account defaults)";
      nginxHTTP01 = lib.mkEnableOption ''
        an HTTP-01-only nginx host for this server's own ACME certificate.
        Requires tls.enableACME and hostName. Serves challenge tokens on port 80,
        refuses other HTTP paths and creates no HTTPS host. The certificate/key
        stay private to ACME and the selected-pair publisher, not nginx. Configure
        the ACME account and inbound port 80 separately. Other challenge methods
        remain available without nginx when this option is false
      '';
      useACMEHost = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "voice.example.org";
        description = "Reuse this security.acme.certs entry, including one provisioned by nginx enableACME. The connection hostname defaults to that entry's domain; set hostName explicitly for a wildcard or a different covered SAN.";
      };
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
        message = "Nuschl PostgreSQL requires database.urlFile or database.createLocally.";
      }
      {
        assertion = !cfg.database.createLocally || cfg.database.type == "postgresql";
        message = "Nuschl database.createLocally requires PostgreSQL.";
      }
      {
        assertion = !cfg.database.createLocally || cfg.database.urlFile == null;
        message = "Choose local peer authentication or database.urlFile, not both.";
      }
      {
        assertion = !cfg.tls.enableACME || cfg.hostName != null;
        message = "Nuschl tls.enableACME requires hostName.";
      }
      {
        assertion = !cfg.tls.nginxHTTP01 || (cfg.tls.enableACME && cfg.hostName != null);
        message = "Nuschl tls.nginxHTTP01 requires tls.enableACME and hostName.";
      }
      {
        assertion =
          !useHTTP01
          || (
            !http01Vhost.enableACME
            && http01Vhost.useACMEHost == null
            && !http01Vhost.addSSL
            && !http01Vhost.onlySSL
            && !http01Vhost.forceSSL
            && !http01Vhost.rejectSSL
            && lib.all (listen: listen.port == 80 && !listen.ssl) http01Vhost.listen
          );
        message = "Nuschl tls.nginxHTTP01 needs a dedicated HTTP-only host; do not merge HTTPS/certificate settings into that nginx virtual host.";
      }
      {
        assertion = !useHTTP01 || config.security.acme.certs.${http01Host}.group == "nuschl-acme";
        message = "Nuschl's HTTP-01-only certificate must retain its private nuschl-acme group, not the nginx/server group.";
      }
      {
        assertion = !(cfg.tls.enableACME && cfg.tls.useACMEHost != null);
        message = "Choose Nuschl tls.enableACME or tls.useACMEHost, not both.";
      }
      {
        assertion = !useACME || (cfg.tls.certificateFile == null && cfg.tls.keyFile == null);
        message = "Nuschl ACME TLS cannot be combined with explicit certificateFile/keyFile.";
      }
      {
        assertion =
          !useACME
          || (tlsHost != "" && builtins.match "[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?" tlsHost != null);
        message = "Nuschl ACME TLS requires a concrete DNS hostName (set it explicitly for a wildcard or differently named ACME entry).";
      }
      {
        assertion = (cfg.tls.certificateFile == null) == (cfg.tls.keyFile == null);
        message = "Nuschl TLS requires both certificateFile and keyFile.";
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
        message = "Nuschl secrets (tls.keyFile, database.urlFile, passwordFile, superUserPasswordFile, kt.signingKeyFile, kt.vrfSecretFile) must not be in the world-readable Nix store.";
      }
      {
        assertion =
          !(cfg.settings ? kt && (cfg.settings.kt ? signing_key_file || cfg.settings.kt ? vrf_secret_file));
        message = "Use services.nuschl.kt.signingKeyFile / vrfSecretFile instead of key paths in settings.";
      }
      {
        assertion =
          !(
            cfg.settings ? password || cfg.settings ? password_file || cfg.settings ? superuser_password_file
          );
        message = "Use services.nuschl.passwordFile / superUserPasswordFile instead of secrets in settings.";
      }
    ];
    security.acme.certs = lib.mkIf useACME {
      ${acmeHost} = {
        postRun = lib.mkAfter ''
          ${pkgs.systemd}/bin/systemctl --no-block start nuschl-tls-refresh.service
        '';
      }
      // lib.optionalAttrs useHTTP01 {
        webroot = http01Webroot;
        # Override inherited challenge defaults only for this selected cert.
        # Explicit incompatible per-cert definitions conflict rather than being
        # silently discarded with mkForce. Other certificates are untouched.
        dnsProvider = null;
        listenHTTP = null;
        s3Bucket = null;
        group = "nuschl-acme";
      };
    };
    users.groups.nuschl-acme = lib.mkIf useHTTP01 { };
    services.nginx = lib.mkIf useHTTP01 {
      enable = true;
      virtualHosts.${http01Host} = {
        listen = [
          {
            addr = "0.0.0.0";
            port = 80;
          }
        ]
        ++ lib.optional config.networking.enableIPv6 {
          addr = "[::]";
          port = 80;
        };
        # Do NOT enable nginx's ACME integration: it grants certificate access
        # to nginx and collects the cert into HTTPS service dependencies.
        locations."^~ /.well-known/acme-challenge/" = {
          root = http01Webroot;
          tryFiles = "$uri =404";
          extraConfig = ''
            auth_basic off;
            auth_request off;
            limit_except GET { deny all; }
          '';
        };
        # A server-level return would also reject the challenge location.
        locations."/".return = "404";
      };
    };
    # The stable certificate setup unit precedes issuance on both old and split
    # order/renew layouts. This host is NOT an nginx SSL dependency, so waiting
    # for nginx here does not create a certificate/startup cycle.
    systemd.services."acme-${http01Host}" = lib.mkIf useHTTP01 {
      wants = [
        "nginx.service"
      ]
      ++ lib.optional config.services.nginx.enableReload "nginx-config-reload.service";
      after = [
        "nginx.service"
      ]
      ++ lib.optional config.services.nginx.enableReload "nginx-config-reload.service";
    };
    systemd.services.nginx = lib.mkIf useHTTP01 {
      serviceConfig.InaccessiblePaths = [
        "-${acmeDirectory}"
        "-${tlsRuntime}"
      ];
    };
    users.groups.nuschl-tls = lib.mkIf useACME { };
    users.users.nuschl-tls = lib.mkIf useACME {
      isSystemUser = true;
      group = "nuschl-tls";
      description = "Nuschl selected TLS credential publisher";
    };
    systemd.services.nuschl-tls-refresh = lib.mkIf useACME {
      description = "Validate and privately publish Nuschl's selected ACME TLS pair";
      # This unit exists before and after NixOS split issuance into a separate
      # acme-order-renew service. Newer releases removed acme-finished targets.
      # A bootstrap pair is still refused by the publisher's trust validation;
      # the actual issuance post-run hook and timer retry publication later.
      after = [ "acme-${acmeHost}.service" ];
      requires = [ "acme-${acmeHost}.service" ];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "30s";
        MemoryMax = "128M";
        TasksMax = 16;
        User = "nuschl-tls";
        Group = "nuschl-tls";
        UMask = "0077";
        RuntimeDirectory = "nuschl-tls";
        RuntimeDirectoryMode = "0750";
        RuntimeDirectoryPreserve = "yes";
        LoadCredential = [
          "fullchain:${acmeDirectory}/fullchain.pem"
          "key:${acmeDirectory}/key.pem"
        ];
        ExecStart = refreshTLS;
        # This fixed command is the only privileged step; it never reads keys.
        ExecStartPost = "+${signalTLS}";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        PrivateNetwork = true;
        PrivateIPC = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        RestrictAddressFamilies = [ "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
      };
    };
    # A renewal hook can coincide with an already-running refresh. This bounded
    # retry also recovers after a temporarily incomplete/invalid source pair.
    systemd.timers.nuschl-tls-refresh = lib.mkIf useACME {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "5min";
        OnUnitInactiveSec = "5min";
        Unit = "nuschl-tls-refresh.service";
      };
    };
    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [ cfg.port ];
      allowedUDPPorts = [ cfg.port ];
    };
    services.postgresql = lib.mkIf localPostgres {
      enable = true;
      ensureDatabases = [ "nuschl" ];
      ensureUsers = [
        {
          name = "nuschl";
          ensureDBOwnership = true;
        }
      ];
    };
    systemd.services.nuschl = {
      description = "Nuschl Mumble-compatible server";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
      ]
      ++ lib.optional localPostgres "postgresql.target"
      ++ lib.optional useACME "nuschl-tls-refresh.service";
      # A failed provisioning attempt must not cancel the server's start job:
      # the readiness gate below fails closed and Restart retries it. A valid
      # previously published pair remains usable if a fresh update is rejected.
      wants = lib.optional useACME "nuschl-tls-refresh.service";
      requires = lib.optional localPostgres "postgresql.target";
      unitConfig = lib.optionalAttrs useACME {
        StartLimitIntervalSec = 0;
      };
      restartTriggers = [ configFile ];
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        User = "nuschl";
        StateDirectory = "nuschl";
        StateDirectoryMode = "0700";
        WorkingDirectory = stateDirectory;
        UMask = "0077";
        SupplementaryGroups = lib.optional useACME "nuschl-tls";
        ReadOnlyPaths = lib.optional useACME tlsRuntime;
        InaccessiblePaths = lib.optional useHTTP01 "-${acmeDirectory}";
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
        # under the operator lock: never while `nuschld admin` (or an import)
        # uses the database.
        # Do not run database checks/migrations while initial issuance is
        # unavailable. The server cannot fall back to a self-signed identity:
        # tlsArguments always names the helper's validated runtime pair.
        ExecStartPre = lib.optional useACME "${requireTLS} ${tlsRuntime}" ++ [
          (lib.escapeShellArgs (
            [
              "${cfg.package}/bin/nuschl-storage-worker"
              "--server"
              "--check-database"
              "--operator-lock"
            ]
            ++ databaseArguments
          ))
        ];
        ExecStart = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/nuschld"
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
        RestartSec = if useACME then "30s" else "5s";
        LimitNOFILE = 65536;
      }
      // hardening;
    };
    # The one-time Murmur import, before the server's first start (docs/server.md).
    systemd.services.nuschl-import-murmur = lib.mkIf (cfg.importMurmur.databaseFile != null) {
      description = "Import a Murmur database into the Nuschl server, once";
      requiredBy = [ "nuschl.service" ];
      before = [ "nuschl.service" ];
      after = [ "network.target" ] ++ lib.optional localPostgres "postgresql.target";
      requires = lib.optional localPostgres "postgresql.target";
      unitConfig.ConditionPathExists = "!${murmurMarker}";
      serviceConfig = {
        Type = "oneshot";
        DynamicUser = true;
        User = "nuschl";
        StateDirectory = "nuschl";
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
