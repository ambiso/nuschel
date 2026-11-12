{ self }:
# The music bot (docs/musicbot.md): `services.nuschl-musicbot`.
#
#   services.nuschl-musicbot = {
#     enable = true;
#     virtualHost = "music.example.org";
#     enableACME = true;
#     credentials.navidrome = "/run/secrets/navidrome-api-key";
#     settings = {
#       subsonic = [
#         { name = "Navidrome"; url = "https://navidrome.example.org"; api_key_file = "navidrome"; }
#       ];
#       channel = [
#         { name = "lounge"; host = "voice.example.org"; username = "Music"; channel = "Lounge"; }
#       ];
#     };
#   };
#
# Relative `*_file` names in the settings are the credentials above; the bot
# reads them from systemd's credentials directory.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschl-musicbot;
  stateDirectory = "/var/lib/nuschl-musicbot";
  format = pkgs.formats.toml { };
  inlineTokens = lib.any (channel: (channel.tokens or [ ]) != [ ]) (cfg.settings.channel or [ ]);
  rawConfigFile =
    if inlineTokens then
      throw "services.nuschl-musicbot: use channel.tokens_file and credentials for ACL secrets; inline tokens enter the Nix store"
    else
      format.generate "nuschl-musicbot-unchecked.toml" cfg.settings;
  # Deploy only the file accepted by this package's runtime parser. The check
  # reads no credentials, creates no state and makes no network connections.
  configFile = pkgs.runCommand "nuschl-musicbot.toml" { } ''
    ${cfg.package}/bin/nuschl-musicbot --config ${rawConfigFile} --check-config
    install -m0444 ${rawConfigFile} "$out"
  '';
  listen =
    if lib.hasInfix ":" cfg.listenAddress then
      "[${cfg.listenAddress}]:${toString cfg.port}"
    else
      "${cfg.listenAddress}:${toString cfg.port}";
  notInStore = path: !(lib.hasPrefix builtins.storeDir (toString path));
in
{
  options.services.nuschl-musicbot = {
    enable = lib.mkEnableOption "nuschl-musicbot, a Mumble music bot that plays songs from Subsonic servers";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.musicbot;
      defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.musicbot";
      description = "Package containing nuschl-musicbot.";
    };
    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Address of the web page's plain-HTTP listener (nginx proxies to it when virtualHost is set).";
    };
    port = lib.mkOption {
      type = lib.types.port;
      default = 8474;
    };
    virtualHost = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "music.example.org";
      description = "Serve the web page through nginx under this host name.";
    };
    enableACME = lib.mkEnableOption "an ACME certificate for virtualHost (HTTPS; set security.acme.acceptTerms and an email)";
    openFirewall = lib.mkEnableOption "opening 80 and 443 (with virtualHost) or the listener's port";
    credentials = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        navidrome = "/run/secrets/navidrome-api-key";
        voice = "/run/secrets/mumble-password";
      };
      description = ''
        Secret files (API keys and passwords) passed with LoadCredential. A
        setting such as `api_key_file = "navidrome"` names one of them. Paths
        must be outside the Nix store.
      '';
    };
    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = format.type;
        options.web.listen = lib.mkOption {
          type = lib.types.str;
          default = listen;
          defaultText = lib.literalExpression ''"''${listenAddress}:''${port}"'';
          description = "The web page's listener; set listenAddress and port instead.";
        };
        options.web.public_url = lib.mkOption {
          type = lib.types.str;
          default =
            if cfg.virtualHost != null then
              "${if cfg.enableACME then "https" else "http"}://${cfg.virtualHost}"
            else
              "http://${listen}";
          defaultText = lib.literalExpression ''"https://''${virtualHost}"'';
          description = "Where people reach the web page; the bot's links start with it.";
        };
      };
      default = { };
      description = ''
        The configuration (see nuschl-musicbot.example.toml): `subsonic` and
        `channel` lists, and optionally `audio`. The generated TOML is checked
        by the configured package during the build, using its runtime schema
        and validation rules. Credential files and servers are not accessed.
        `web.listen` comes from listenAddress and port.
      '';
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !inlineTokens;
        message = "services.nuschl-musicbot: use channel.tokens_file with credentials, not inline ACL tokens.";
      }
      {
        assertion = lib.all notInStore (lib.attrValues cfg.credentials);
        message = "services.nuschl-musicbot.credentials must not be in the world-readable Nix store.";
      }
      {
        assertion = !cfg.enableACME || cfg.virtualHost != null;
        message = "services.nuschl-musicbot.enableACME needs virtualHost.";
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
          proxyPass = "http://${listen}";
          # The links are secrets: keep them out of the access log.
          extraConfig = "access_log off;";
        };
      };
    };
    systemd.services.nuschl-musicbot = {
      description = "Nuschl music bot";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      restartTriggers = [ configFile ];
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        User = "nuschl-musicbot";
        StateDirectory = "nuschl-musicbot";
        StateDirectoryMode = "0700";
        WorkingDirectory = stateDirectory;
        UMask = "0077";
        LoadCredential = lib.mapAttrsToList (name: path: "${name}:${path}") cfg.credentials;
        ExecStart = lib.escapeShellArgs [
          "${cfg.package}/bin/nuschl-musicbot"
          "--config"
          configFile
          "--state-directory"
          stateDirectory
        ];
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
        # Mumble over TCP and UDP, the libraries over HTTP(S), DNS through the system resolver.
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
        # The web listener, and the UDP sockets of the voice connections.
        SocketBindAllow = [
          "tcp:${toString cfg.port}"
          "udp"
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
