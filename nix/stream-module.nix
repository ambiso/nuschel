# The screen-share stream host (screen-share plan §5, #143): the static watch
# and share pages and the SFU's signalling WebSocket (/sfu) on one origin, with
# the isolation headers from the plan and no cookies. `services.nuschl-stream`.
#
# The SFU does not exist yet. Its unit is only created when sfu.enable is set,
# which requires sfu.package. The command line and the configuration file
# below are the provisional contract for it:
#   - `<package main program> --config <nuschl-sfu.toml>`;
#   - signalling arrives on an inherited, socket-activated Unix stream socket
#     (LISTEN_FDS) that only nginx may connect to, carrying the client address
#     in X-Real-IP and the page origin in Origin;
#   - media uses the one UDP port in the configuration (the only port it may
#     bind), and the TURN REST secret, when set, is the systemd credential
#     named in [turn] (one line; a trailing newline is not part of it).
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschl-stream;
  inherit (lib) mkOption types;
  toml = pkgs.formats.toml { };
  https = cfg.forceSSL || cfg.enableACME || cfg.useACMEHost != null;
  origin = "${if https then "https" else "http"}://${cfg.hostName}";
  socket = "/run/nuschl-stream/sfu.sock";
  notInStore = path: path == null || !(lib.hasPrefix builtins.storeDir (toString path));
  firewallPorts = {
    allowedUDPPorts = [ cfg.sfu.mediaPort ];
  };

  # Same origin only. The explicit ws(s) source covers browsers that do not
  # count WebSockets as 'self'.
  contentSecurityPolicy = lib.concatStringsSep "; " [
    "default-src 'none'"
    "script-src 'self' 'wasm-unsafe-eval'"
    "worker-src 'self'"
    "connect-src 'self' ${if https then "wss" else "ws"}://${cfg.hostName}"
    "img-src 'self' blob:"
    "style-src 'self'"
    "base-uri 'none'"
    "form-action 'none'"
    "frame-ancestors 'none'"
    "require-trusted-types-for 'script'"
  ];
  # The pages run no identity code: no passkey ceremonies on this origin.
  noIdentity = "publickey-credentials-get=(), publickey-credentials-create=()";
  watchPermissions = "display-capture=(), camera=(), microphone=(), geolocation=(), ${noIdentity}";
  sharePermissions = "display-capture=(self), camera=(), microphone=(), geolocation=(), ${noIdentity}";
  # nginx drops inherited add_header lines in any block that has its own, so
  # every location repeats the full set.
  headers = permissions: ''
    add_header Content-Security-Policy "${contentSecurityPolicy}" always;
    add_header Cross-Origin-Opener-Policy same-origin always;
    add_header Cross-Origin-Embedder-Policy require-corp always;
    add_header Cross-Origin-Resource-Policy same-origin always;
    add_header Origin-Agent-Cluster "?1" always;
    add_header Permissions-Policy "${permissions}" always;
    add_header Referrer-Policy no-referrer always;
    add_header X-Content-Type-Options nosniff always;
    ${lib.optionalString https ''add_header Strict-Transport-Security "max-age=31536000" always;''}
  '';

  # Loopback, private, CGNAT, link-local and multicast: the SFU must not probe
  # them on behalf of a client's ICE candidates.
  refusedNetworks = [
    "localhost"
    "link-local"
    "multicast"
    "0.0.0.0/8"
    "10.0.0.0/8"
    "100.64.0.0/10"
    "172.16.0.0/12"
    "192.0.0.0/24"
    "192.168.0.0/16"
    "198.18.0.0/15"
    "240.0.0.0/4"
    "fc00::/7"
    "fec0::/10"
  ];
  privilegedPort = cfg.sfu.mediaPort < 1024;
  sfuConfig = toml.generate "nuschl-sfu.toml" (
    lib.recursiveUpdate (
      {
        signalling = "systemd-socket";
        media_port = cfg.sfu.mediaPort;
        public_addresses = cfg.sfu.publicAddresses;
        allowed_origins = cfg.sfu.allowedOrigins;
        # Project caps (screen-share plan §5, #143).
        limits = {
          publishers_per_room = 1;
          viewers_per_room = 16;
          room_bitrate_bps = 2500000;
          room_lifetime_seconds = 14400;
        };
      }
      // lib.optionalAttrs (cfg.sfu.turn.secretFile != null) {
        turn = {
          inherit (cfg.sfu.turn) uris;
          secret_credential = "turn-rest-secret";
          credential_lifetime_seconds = 600;
        };
      }
    ) cfg.sfu.settings
  );
in
{
  options.services.nuschl-stream = {
    enable = lib.mkEnableOption "the Nuschl screen-share stream host (watch/share pages and SFU signalling)";
    hostName = mkOption {
      type = types.str;
      example = "stream.nuschl.eu";
      description = "The virtual host. Keep it separate from the app and identity origins.";
    };
    enableACME = mkOption {
      type = types.bool;
      default = true;
      description = "Get a certificate for hostName (security.acme must be configured).";
    };
    useACMEHost = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Use this existing security.acme.certs entry instead (for example a wildcard).";
    };
    forceSSL = mkOption {
      type = types.bool;
      default = true;
      description = "Redirect HTTP to HTTPS.";
    };
    site = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = ''
        The static pages: watch.html (served at /watch), share.html (at /share)
        and their assets. Null serves nothing (404 with the same headers).
      '';
    };
    sfu = {
      enable = lib.mkEnableOption "the screen-share SFU behind /sfu (requires sfu.package)";
      package = mkOption {
        type = types.nullOr types.package;
        default = null;
        description = "The SFU; its main program follows the contract at the top of this module.";
      };
      mediaPort = mkOption {
        type = types.port;
        default = 443;
        description = "The SFU's one UDP media port (443 unless nginx serves QUIC).";
      };
      publicAddresses = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "203.0.113.10" ];
        description = "Addresses the SFU announces in its ICE candidates.";
      };
      allowedOrigins = mkOption {
        type = types.listOf types.str;
        default = [ origin ];
        defaultText = lib.literalExpression ''[ "https://''${hostName}" ]'';
        description = "Page origins the SFU accepts in the WebSocket Origin header.";
      };
      allowNetworks = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "192.168.1.0/24" ];
        description = "Private networks the SFU may reach despite the kernel IP filter (LANs, tests).";
      };
      turn = {
        uris = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [
            "turn:turn.nuschl.eu:3478?transport=udp"
            "turns:turn.nuschl.eu:5349?transport=tcp"
          ];
          description = "TURN URIs handed to admitted clients with short-lived REST credentials.";
        };
        secretFile = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "/run/agenix/nuschl-turn-rest-secret";
          description = "Runtime path of the TURN REST secret (services.nuschl-turn.secretFile), imported with LoadCredential.";
        };
      };
      memoryMax = mkOption {
        type = types.str;
        default = "1G";
        description = "systemd MemoryMax for the SFU.";
      };
      settings = mkOption {
        type = toml.type;
        default = { };
        description = "Further nuschl-sfu.toml settings, merged over the generated ones.";
      };
    };
    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Open the SFU's UDP media port (HTTP/HTTPS stay with the nginx configuration).";
    };
    firewallInterfaces = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "ens3" ];
      description = "Open the firewall only on these interfaces; empty means all.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !cfg.sfu.enable || cfg.sfu.package != null;
        message = "services.nuschl-stream.sfu.enable requires services.nuschl-stream.sfu.package.";
      }
      {
        assertion = notInStore cfg.sfu.turn.secretFile;
        message = "services.nuschl-stream.sfu.turn.secretFile must be a runtime path outside the Nix store.";
      }
      {
        assertion = cfg.sfu.turn.uris == [ ] || cfg.sfu.turn.secretFile != null;
        message = "services.nuschl-stream.sfu.turn.uris requires sfu.turn.secretFile to issue credentials.";
      }
      {
        assertion =
          !(cfg.sfu.enable && cfg.sfu.mediaPort == 443)
          || !(lib.any (vhost: vhost.quic) (lib.attrValues config.services.nginx.virtualHosts));
        message = "services.nuschl-stream.sfu.mediaPort 443 collides with nginx QUIC; choose another port.";
      }
    ];

    services.nginx = {
      enable = true;
      virtualHosts.${cfg.hostName} = {
        inherit (cfg) enableACME useACMEHost forceSSL;
        root = if cfg.site == null then null else cfg.site;
        # No access log: room ids and client addresses stay out of it.
        extraConfig = ''
          access_log off;
          ${headers watchPermissions}
        '';
        locations = {
          "/" =
            if cfg.site == null then
              {
                return = "404";
                extraConfig = headers watchPermissions;
              }
            else
              {
                tryFiles = "$uri =404";
                extraConfig = headers watchPermissions;
              };
        }
        // lib.optionalAttrs (cfg.site != null) {
          "= /watch" = {
            tryFiles = "/watch.html =404";
            extraConfig = headers watchPermissions;
          };
          "= /share" = {
            tryFiles = "/share.html =404";
            extraConfig = headers sharePermissions;
          };
        }
        // lib.optionalAttrs cfg.sfu.enable {
          "= /sfu" = {
            proxyPass = "http://unix:${socket}:";
            proxyWebsockets = true;
            recommendedProxySettings = false;
            extraConfig = ''
              ${headers watchPermissions}
              limit_except GET { deny all; }
              proxy_set_header Host $host;
              proxy_set_header X-Real-IP $remote_addr;
              proxy_set_header Cookie "";
              proxy_set_header Authorization "";
              proxy_hide_header Set-Cookie;
              proxy_buffering off;
              proxy_read_timeout 1h;
              proxy_send_timeout 1h;
              client_max_body_size 64k;
            '';
          };
        };
      };
    };

    networking.firewall = lib.mkIf (cfg.openFirewall && cfg.sfu.enable) (
      if cfg.firewallInterfaces == [ ] then
        firewallPorts
      else
        { interfaces = lib.genAttrs cfg.firewallInterfaces (_: firewallPorts); }
    );

    # Only nginx can open the signalling socket; the SFU starts on demand.
    systemd.sockets.nuschl-sfu = lib.mkIf cfg.sfu.enable {
      description = "Nuschl screen-share SFU signalling socket";
      wantedBy = [ "sockets.target" ];
      listenStreams = [ socket ];
      socketConfig = {
        SocketUser = "root";
        SocketGroup = config.services.nginx.group;
        SocketMode = "0660";
        DirectoryMode = "0755";
        RemoveOnStop = true;
      };
    };

    systemd.services.nuschl-sfu = lib.mkIf cfg.sfu.enable {
      description = "Nuschl screen-share SFU";
      requires = [ "nuschl-sfu.socket" ];
      after = [
        "network.target"
        "nuschl-sfu.socket"
      ];
      restartTriggers = [ sfuConfig ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs [
          (lib.getExe cfg.sfu.package)
          "--config"
          sfuConfig
        ];
        DynamicUser = true;
        LoadCredential = lib.optional (
          cfg.sfu.turn.secretFile != null
        ) "turn-rest-secret:${cfg.sfu.turn.secretFile}";
        Restart = "on-failure";
        RestartSec = "2s";
        UMask = "0077";
        MemoryMax = cfg.sfu.memoryMax;
        TasksMax = 256;
        LimitNOFILE = 16384;
        # It binds nothing but its media port.
        SocketBindDeny = "any";
        SocketBindAllow = [ "udp:${toString cfg.sfu.mediaPort}" ];
        IPAddressDeny = refusedNetworks;
        IPAddressAllow = cfg.sfu.allowNetworks;
        AmbientCapabilities = lib.optional privilegedPort "CAP_NET_BIND_SERVICE";
        CapabilityBoundingSet = if privilegedPort then [ "CAP_NET_BIND_SERVICE" ] else [ "" ];
        # A private user namespace cannot bind privileged ports on the host.
        PrivateUsers = !privilegedPort;
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
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RemoveIPC = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged @resources"
        ];
      };
    };
  };
}
