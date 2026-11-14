# The screen-share TURN relay (#143): coturn with TURN REST credentials
# (use-auth-secret) that only the SFU issues, after it admitted a link. Every
# peer is denied except the SFU's addresses, so leaked credentials cannot turn
# this into an open relay. `services.nuschl-turn`; ports, listening and relay
# addresses stay in the upstream `services.coturn` options.
#
# The REST secret and the TLS pair reach coturn as systemd credentials (PID 1
# reads them); none of them is in the Nix store.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschl-turn;
  inherit (lib) mkOption types;
  credentials = "/run/credentials/coturn.service";
  notInStore = path: path == null || !(lib.hasPrefix builtins.storeDir path);
  useFiles = cfg.tls.certificateFile != null;
  useTLS = cfg.tls.useACMEHost != null || useFiles;
  acmeDirectory = config.security.acme.certs.${cfg.tls.useACMEHost}.directory;
  peers =
    family:
    lib.filter (address: lib.hasInfix ":" address == (family == "ip6")) cfg.allowedPeerAddresses;
  set = values: "{ ${lib.concatMapStringsSep ", " toString values} }";
  # coturn cannot restrict ports, so nftables drops its other packets to the SFU.
  portRules =
    family:
    lib.optionalString (peers family != [ ]) ''
      meta skuid ${toString config.ids.uids.turnserver} ${family} daddr ${set (peers family)} udp dport != ${set cfg.allowedPeerPorts} counter drop
    '';
in
{
  options.services.nuschl-turn = {
    enable = lib.mkEnableOption "the Nuschl screen-share TURN relay";
    hostName = mkOption {
      type = types.str;
      example = "turn.nuschl.eu";
      description = "The TURN realm.";
    };
    secretFile = mkOption {
      type = types.str;
      example = "/run/agenix/nuschl-turn-rest-secret";
      description = "Runtime path of the REST secret (one line) shared with the SFU.";
    };
    allowedPeerAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "203.0.113.10" ];
      description = "The SFU's addresses: the only peers coturn relays to. Empty relays nothing.";
    };
    allowedPeerPorts = mkOption {
      type = types.nullOr (types.listOf types.port);
      default = null;
      example = [ 443 ];
      description = ''
        The SFU's UDP ports. Set it when the SFU shares a host with other UDP
        services (relayed packets then arrive over loopback, past the input
        firewall). Requires networking.nftables.enable.
      '';
    };
    userQuota = mkOption {
      type = types.ints.positive;
      default = 4;
      description = "Concurrent allocations per REST username.";
    };
    totalQuota = mkOption {
      type = types.ints.positive;
      default = 128;
      description = "Concurrent allocations in total.";
    };
    maxBps = mkOption {
      type = types.ints.positive;
      default = 500000;
      description = "Bytes per second per session (500000 is 4 Mbit/s).";
    };
    openFirewall = lib.mkEnableOption "the TURN ports and the relay port range";
    tls = {
      useACMEHost = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "The security.acme.certs entry to serve TLS with.";
      };
      certificateFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Runtime PEM certificate chain (instead of useACMEHost).";
      };
      keyFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Runtime PEM private key (instead of useACMEHost).";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.all notInStore [
          cfg.secretFile
          cfg.tls.keyFile
        ];
        message = "services.nuschl-turn secrets must be runtime paths outside the Nix store.";
      }
      {
        assertion = config.services.coturn.static-auth-secret == null;
        message = "Use services.nuschl-turn.secretFile, not services.coturn.static-auth-secret.";
      }
      {
        assertion = useFiles == (cfg.tls.keyFile != null) && !(useFiles && cfg.tls.useACMEHost != null);
        message = "services.nuschl-turn.tls takes useACMEHost or both certificateFile and keyFile.";
      }
      {
        assertion = cfg.allowedPeerPorts == null || config.networking.nftables.enable;
        message = "services.nuschl-turn.allowedPeerPorts requires networking.nftables.enable.";
      }
    ];

    services.coturn = {
      enable = true;
      realm = cfg.hostName;
      use-auth-secret = true;
      static-auth-secret-file = "${credentials}/rest-secret";
      no-tcp-relay = true;
      no-tls = !useTLS;
      secure-stun = lib.mkDefault true;
      cert = if useTLS then "${credentials}/tls-certificate" else null;
      pkey = if useTLS then "${credentials}/tls-key" else null;
      # Deny every peer, then allow the SFU: allowed-peer-ip overrides
      # denied-peer-ip, but never allow-loopback-peers, which stays unset.
      # coturn 4.17 starts no CLI unless asked.
      extraConfig = ''
        no-multicast-peers
        denied-peer-ip=0.0.0.0-255.255.255.255
        denied-peer-ip=::-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
        ${lib.concatMapStrings (address: "allowed-peer-ip=${address}\n") cfg.allowedPeerAddresses}
        no-dynamic-ip-list
        no-dynamic-realms
        user-quota=${toString cfg.userQuota}
        total-quota=${toString cfg.totalQuota}
        max-bps=${toString cfg.maxBps}
        log-file=stdout
        simple-log
        log-min-level=warning
      '';
    };

    systemd.services.coturn = {
      wants = lib.optional (cfg.tls.useACMEHost != null) "acme-${cfg.tls.useACMEHost}.service";
      after = lib.optional (cfg.tls.useACMEHost != null) "acme-${cfg.tls.useACMEHost}.service";
      serviceConfig = {
        LoadCredential = [
          "rest-secret:${cfg.secretFile}"
        ]
        ++ lib.optionals useTLS [
          "tls-certificate:${if useFiles then cfg.tls.certificateFile else "${acmeDirectory}/fullchain.pem"}"
          "tls-key:${if useFiles then cfg.tls.keyFile else "${acmeDirectory}/key.pem"}"
        ];
        # Addresses and usernames stay in a small journal kept for two days.
        LogNamespace = "nuschl-turn";
      };
    };
    environment.etc."systemd/journald@nuschl-turn.conf".text = ''
      [Journal]
      Storage=volatile
      RuntimeMaxUse=32M
      MaxRetentionSec=2day
    '';
    # coturn reads its credentials at start, so a renewal restarts it.
    security.acme.certs = lib.mkIf (cfg.tls.useACMEHost != null) {
      ${cfg.tls.useACMEHost}.reloadServices = [ "coturn.service" ];
    };

    networking.nftables.tables.nuschl-turn-peers = lib.mkIf (cfg.allowedPeerPorts != null) {
      family = "inet";
      content = ''
        chain output {
          type filter hook output priority filter;
          ${portRules "ip"}
          ${portRules "ip6"}
        }
      '';
    };

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedUDPPorts = [ config.services.coturn.listening-port ];
      allowedTCPPorts = [
        config.services.coturn.listening-port
      ]
      ++ lib.optional useTLS config.services.coturn.tls-listening-port;
      allowedUDPPortRanges = [
        {
          from = config.services.coturn.min-port;
          to = config.services.coturn.max-port;
        }
      ];
    };
  };
}
