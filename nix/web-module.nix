# The web host (docs/web.md): the web client's static files, the share-link
# landing page, the app-link files for Android and iOS, and the tunnel relay
# behind nginx, on one virtual host with ACME. `services.nuschl-web`.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschl-web;
  toml = pkgs.formats.toml { };
  json = pkgs.formats.json { };
  system = pkgs.stdenv.hostPlatform.system;
  defaultInstallUrl = "https://github.com/ambiso/nuschl/releases/latest";
  https = cfg.forceSSL || cfg.enableACME;
  origin = "${if https then "https" else "http"}://${cfg.hostName}";
  socketDirectory = "/run/nuschl-web-relay";
  socket = "${socketDirectory}/relay.sock";
  # The site with this host's install link.
  site =
    if cfg.installUrl == defaultInstallUrl then
      cfg.package
    else
      pkgs.runCommand "nuschl-web-site-configured" { } ''
        cp -R ${cfg.package} "$out"
        chmod -R u+w "$out"
        for page in "$out"/*.html; do
          substituteInPlace "$page" --replace-quiet ${lib.escapeShellArg defaultInstallUrl} ${lib.escapeShellArg cfg.installUrl}
        done
      '';
  # Android App Links (Digital Asset Links) and iOS Universal Links: the apps
  # open /join/ links themselves. Empty lists by default.
  assetLinks = json.generate "assetlinks.json" (
    map (app: {
      relation = [ "delegate_permission/common.handle_all_urls" ];
      target = {
        namespace = "android_app";
        package_name = app.package;
        sha256_cert_fingerprints = app.certificateFingerprints;
      };
    }) cfg.appLinks.android
  );
  appleAssociation = json.generate "apple-app-site-association" {
    applinks.details = map (appID: {
      appIDs = [ appID ];
      components = [ { "/" = "/join/*"; } ];
    }) cfg.appLinks.apple;
  };
  relayConfig = toml.generate "nuschl-web-relay.toml" (
    {
      listen = "unix:${socket}";
      ports = cfg.relay.ports;
      allow_networks = cfg.relay.allowNetworks;
      allowed_origins = cfg.relay.allowedOrigins;
      require_ping = cfg.relay.requirePing;
    }
    // cfg.relay.settings
  );
  # No tracking, no third parties: everything comes from this origin. The
  # relay is the only connection the web client makes ('self', with ws: and
  # wss: for older browsers that do not count them as 'self').
  contentSecurityPolicy = lib.concatStringsSep "; " [
    "default-src 'none'"
    "script-src 'self' 'wasm-unsafe-eval'"
    "style-src 'self'"
    "img-src 'self' data: blob:"
    "font-src 'self'"
    "connect-src 'self' ${if https then "wss" else "ws"}://${cfg.hostName}"
    "worker-src 'self'"
    "manifest-src 'self'"
    "base-uri 'none'"
    "form-action 'none'"
    "frame-ancestors 'none'"
  ];
  headers = ''
    add_header Content-Security-Policy "${contentSecurityPolicy}" always;
    add_header Referrer-Policy no-referrer always;
    add_header X-Content-Type-Options nosniff always;
    add_header Cross-Origin-Opener-Policy same-origin always;
    add_header Permissions-Policy "camera=(), geolocation=(), microphone=(self)" always;
  '';
  # Refused to the relay by the kernel as well as by its own checks (docs/web.md).
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
in
{
  imports = [ (lib.mkRenamedOptionModule [ "services" "nuschel-web" ] [ "services" "nuschl-web" ]) ];
  options.services.nuschl-web = {
    enable = lib.mkEnableOption "the Nuschl web host: web client, share links and tunnel relay";
    hostName = lib.mkOption {
      type = lib.types.str;
      example = "web.nuschl.example";
      description = ''
        The virtual host (a domain or subdomain). Browsers keep the web client's
        identity per origin, so keep one host name for good.
      '';
    };
    enableACME = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Get a certificate from Let's Encrypt (security.acme must be configured).";
    };
    forceSSL = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Redirect HTTP to HTTPS. Browsers allow the web client's WebAssembly and microphone only on HTTPS.";
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.web-site;
      defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.web-site";
      description = "The static files: the web client and the share-link landing page (join.html).";
    };
    installUrl = lib.mkOption {
      type = lib.types.str;
      default = defaultInstallUrl;
      description = "Where \"Get Nuschl\" leads.";
    };
    appLinks = {
      android = lib.mkOption {
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              package = lib.mkOption {
                type = lib.types.str;
                example = "org.nuschl.android";
              };
              certificateFingerprints = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                example = [
                  "14:6D:E9:83:C5:73:06:50:D8:EE:B9:95:2F:34:FC:64:16:A0:83:42:E6:1D:BE:A8:8A:04:96:B2:3F:CF:44:E5"
                ];
                description = "SHA-256 fingerprints of the app's signing certificates.";
              };
            };
          }
        );
        default = [ ];
        description = "Android apps that open this host's /join/ links (/.well-known/assetlinks.json).";
      };
      apple = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "ABCDE12345.org.nuschl.Nuschl" ];
        description = "iOS app ids (team id and bundle id) that open this host's /join/ links (/.well-known/apple-app-site-association).";
      };
    };
    relay = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "The tunnel relay at /relay, through which the web client reaches Mumble and Nuschl servers.";
      };
      package = lib.mkOption {
        type = lib.types.package;
        default = self.packages.${system}.web-relay;
        defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.web-relay";
      };
      ports = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "64738-64799" ];
        example = [
          "64738-64799"
          "10000-20000"
        ];
        description = "Destination ports the relay connects to.";
      };
      allowNetworks = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "192.168.1.0/24" ];
        description = ''
          Networks the relay may reach although they are private or special
          (a LAN server, tests). Public servers need nothing here.
        '';
      };
      resolverAddresses = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = lib.optionals config.services.resolved.enable [ "127.0.0.53" ];
        defaultText = lib.literalExpression ''lib.optionals config.services.resolved.enable [ "127.0.0.53" ]'';
        example = [ "127.0.0.55" ];
        description = ''
          DNS resolver addresses exempted from the relay unit's kernel IP filter.
          This does not permit the relay to tunnel to those addresses: its own
          destination checks still apply. Systemd's IP allowance covers every
          port on each address, so prefer a resolver on a dedicated address.
          Set this for a local or private resolver such as Unbound, if needed.
          The resolver must already listen there and be configured in the
          system's DNS client; this option does not start or select a resolver.
        '';
      };
      allowedOrigins = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ origin ];
        defaultText = lib.literalExpression ''[ "https://''${hostName}" ]'';
        description = "Pages that may open tunnels; empty allows any.";
      };
      requirePing = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Only servers that answer a Mumble UDP ping; otherwise a TLS handshake is proof enough.";
      };
      settings = lib.mkOption {
        type = toml.type;
        default = { };
        example = {
          client_tunnels = 8;
          bytes_per_second = 262144;
        };
        description = "Further nuschl-web-relay.toml settings (apps/web-relay/src/config.rs).";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    services.nginx = {
      enable = true;
      virtualHosts.${cfg.hostName} = {
        inherit (cfg) enableACME forceSSL;
        root = site;
        extraConfig = headers;
        locations = {
          "/".tryFiles = "$uri $uri/ /index.html";
          # The token in the path opens the landing page; no access log keeps it.
          "/join/" = {
            tryFiles = "/join.html =404";
            extraConfig = ''
              access_log off;
            '';
          };
          "= /.well-known/assetlinks.json" = {
            alias = assetLinks;
            extraConfig = ''
              default_type application/json;
            '';
          };
          "= /.well-known/apple-app-site-association" = {
            alias = appleAssociation;
            extraConfig = ''
              default_type application/json;
            '';
          };
        }
        // lib.optionalAttrs cfg.relay.enable {
          "= /relay" = {
            proxyPass = "http://unix:${socket}:";
            proxyWebsockets = true;
            extraConfig = ''
              proxy_set_header Host $host;
              proxy_set_header X-Real-IP $remote_addr;
              proxy_buffering off;
              proxy_read_timeout 1h;
              proxy_send_timeout 1h;
              access_log off;
            '';
          };
        };
      };
    };

    # A system user (not a dynamic one: nginx must share its group to reach the
    # socket). It owns nothing but the socket's directory.
    users.users = lib.mkIf cfg.relay.enable {
      nuschl-web-relay = {
        isSystemUser = true;
        group = "nuschl-web-relay";
      };
      ${config.services.nginx.user}.extraGroups = [ "nuschl-web-relay" ];
    };
    users.groups.nuschl-web-relay = lib.mkIf cfg.relay.enable { };

    systemd.services.nuschl-web-relay = lib.mkIf cfg.relay.enable {
      description = "Nuschl web client tunnel relay";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      before = [ "nginx.service" ];
      restartTriggers = [ relayConfig ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs [
          "${cfg.relay.package}/bin/nuschl-web-relay"
          "--config"
          relayConfig
        ];
        User = "nuschl-web-relay";
        Group = "nuschl-web-relay";
        RuntimeDirectory = "nuschl-web-relay";
        RuntimeDirectoryMode = "0750";
        UMask = "0007";
        Restart = "on-failure";
        RestartSec = "2s";
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
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
        # Additional private/special-network boundary. The address-wide DNS/LAN
        # exceptions below do not replace the relay's application checks.
        IPAddressDeny = refusedNetworks;
        # DNS must remain reachable even when the resolver is on loopback.
        # The relay's destination policy still rejects private/local targets.
        IPAddressAllow = lib.unique (cfg.relay.allowNetworks ++ cfg.relay.resolverAddresses);
      };
    };
  };
}
