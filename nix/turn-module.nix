# The screen-share TURN relay (screen-share plan §5, #143): coturn with TURN
# REST credentials (use-auth-secret) that only the SFU issues, after it admitted
# a link. Peers are restricted to the SFU's addresses, so leaked credentials
# cannot turn this into an open relay. `services.nuschl-turn`.
#
# The shared REST secret and the TLS key reach coturn through systemd
# credentials (PID 1 reads them); none of them is ever in the Nix store.
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
  namespace = "nuschl-turn";
  uid = toString config.ids.uids.turnserver;

  notInStore = path: path == null || !(lib.hasPrefix builtins.storeDir (toString path));
  absolute = path: path == null || lib.hasPrefix "/" path;

  # Peer addresses are single literal addresses; coturn ranges would make the
  # special-range check below unsound.
  ipv4Octets =
    address:
    let
      parts = builtins.match "([0-9]{1,3})\\.([0-9]{1,3})\\.([0-9]{1,3})\\.([0-9]{1,3})" address;
    in
    if parts == null then null else map lib.toInt parts;
  isIPv4 =
    address:
    let
      octets = ipv4Octets address;
    in
    octets != null && lib.all (octet: octet <= 255) octets;
  isIPv6 = address: builtins.match "[0-9a-fA-F:]+" address != null && lib.hasInfix ":" address;
  # Loopback, private, CGNAT, link-local, benchmarking, reserved and multicast.
  specialIPv4 =
    address:
    let
      octets = ipv4Octets address;
      a = builtins.elemAt octets 0;
      b = builtins.elemAt octets 1;
      c = builtins.elemAt octets 2;
    in
    a == 0
    || a == 10
    || a == 127
    || a >= 224
    || (a == 100 && b >= 64 && b <= 127)
    || (a == 169 && b == 254)
    || (a == 172 && b >= 16 && b <= 31)
    || (a == 192 && b == 168)
    || (a == 192 && b == 0 && (c == 0 || c == 2))
    || (a == 198 && (b == 18 || b == 19))
    || (a == 198 && b == 51 && c == 100)
    || (a == 203 && b == 0 && c == 113);
  # Only global unicast (2000::/3) outside the documentation prefix is public.
  specialIPv6 =
    address:
    let
      lower = lib.toLower address;
      first = builtins.head (lib.splitString ":" lower);
      value = if first == "" then 0 else lib.fromHexString first;
    in
    value < 8192 || value > 16383 || lib.hasPrefix "2001:db8:" lower;
  validPeer = address: isIPv4 address || isIPv6 address;
  publicPeer =
    address:
    if isIPv4 address then !(specialIPv4 address) else isIPv6 address && !(specialIPv6 address);
  peers4 = lib.filter isIPv4 cfg.allowedPeerAddresses;
  peers6 = lib.filter isIPv6 cfg.allowedPeerAddresses;

  acmeHost = if cfg.tls.enableACME then cfg.hostName else cfg.tls.useACMEHost;
  useACME = acmeHost != null;
  acmeDirectory = if useACME then config.security.acme.certs.${acmeHost}.directory else "";
  useFiles = cfg.tls.certificateFile != null || cfg.tls.keyFile != null;
  useTLS = useACME || useFiles;
  useHTTP01 = cfg.tls.enableACME && cfg.tls.nginxHTTP01;
  http01Webroot = "/var/lib/nuschl-turn-acme-challenge";

  firewallPorts = {
    allowedUDPPorts = [ cfg.port ];
    allowedTCPPorts = [ cfg.port ] ++ lib.optional useTLS cfg.tlsPort;
    allowedUDPPortRanges = [ cfg.relayPorts ];
  };
  nftSet = values: "{ ${lib.concatStringsSep ", " (map toString values)} }";
  peerPortRules =
    family: peers:
    lib.optionalString (peers != [ ]) ''
      meta skuid ${uid} ${family} daddr ${nftSet peers} udp dport ${nftSet cfg.allowedPeerPorts} accept
      meta skuid ${uid} ${family} daddr ${nftSet peers} counter drop
    '';
in
{
  options.services.nuschl-turn = {
    enable = lib.mkEnableOption "the Nuschl screen-share TURN relay (coturn)";
    hostName = mkOption {
      type = types.str;
      example = "turn.nuschl.eu";
      description = "The TURN host name: the REST realm and the certificate's name.";
    };
    secretFile = mkOption {
      type = types.str;
      example = "/run/agenix/nuschl-turn-rest-secret";
      description = ''
        Runtime path of the TURN REST shared secret (one line), shared with the
        SFU that issues credentials after link admission. Imported with systemd
        LoadCredential; it must not be in the Nix store.
      '';
    };
    allowedPeerAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "203.0.113.10"
        "2001:db8::10"
      ];
      description = ''
        The SFU's media addresses: the only peers coturn relays to. Every other
        address (including loopback, private, link-local, CGNAT and multicast)
        is denied. Single literal addresses, no ranges. Empty relays nothing.
      '';
    };
    allowedPeerPorts = mkOption {
      type = types.nullOr (types.listOf types.port);
      default = null;
      example = [ 443 ];
      description = ''
        UDP ports coturn may send to on the allowed peer addresses, enforced in
        an nftables output rule for coturn's user. coturn itself cannot restrict
        ports, so set this when the SFU shares an address with other UDP
        services (the same host: relayed packets then arrive over loopback,
        past the host's input firewall). Requires networking.nftables.enable.
        Null leaves every port of those addresses reachable.
      '';
    };
    allowNonPublicPeers = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Accept private, documentation or other non-global addresses in
        allowedPeerAddresses. Only for test networks and LANs.
      '';
    };
    port = mkOption {
      type = types.port;
      default = 3478;
      description = "TURN over UDP and TCP.";
    };
    tlsPort = mkOption {
      type = types.port;
      default = 5349;
      description = "TURN over TLS (443 needs a dedicated address).";
    };
    relayPorts = {
      from = mkOption {
        type = types.port;
        default = 49152;
      };
      to = mkOption {
        type = types.port;
        default = 49999;
      };
    };
    listenAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "203.0.113.5" ];
      description = "Addresses coturn listens on; empty means all.";
    };
    relayAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Local addresses for relay sockets; empty means the address each client reached.";
    };
    externalAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "203.0.113.5/10.0.0.5" ];
      description = "coturn external-ip mappings (public or public/private) behind 1:1 NAT.";
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
      description = "Bytes per second per session and direction (500000 is 4 Mbit/s).";
    };
    bpsCapacity = mkOption {
      type = types.nullOr types.ints.positive;
      default = null;
      example = 12500000;
      description = "Total bytes per second for all sessions; null leaves it to the quotas.";
    };
    secureStun = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Refuse unauthenticated STUN Binding requests. Relay-only clients need
        none; this keeps the host from being an anonymous reflector.
      '';
    };
    logLevel = mkOption {
      type = types.enum [
        "error"
        "warning"
        "info"
      ];
      default = "warning";
      description = "coturn's minimum log level; per-session lines (addresses, usernames) are info.";
    };
    logRetention = mkOption {
      type = types.str;
      default = "2day";
      description = ''
        How long the volatile journal namespace "${namespace}" keeps coturn's
        log (`journalctl --namespace=${namespace}`).
      '';
    };
    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Open the TURN ports and the relay range.";
    };
    firewallInterfaces = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "ens3" ];
      description = "Open the firewall only on these interfaces; empty means all.";
    };
    tls = {
      enableACME = lib.mkEnableOption "an ACME certificate for hostName (configure security.acme's account defaults)";
      nginxHTTP01 = lib.mkEnableOption ''
        an HTTP-01-only nginx host for this certificate. It serves challenge
        tokens on port 80 and refuses everything else; nginx gets no access to
        the certificate or key
      '';
      useACMEHost = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Reuse this security.acme.certs entry (for example a wildcard certificate).";
      };
      certificateFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Runtime PEM certificate chain, imported with LoadCredential.";
      };
      keyFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Runtime PEM private key, imported with LoadCredential.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = absolute cfg.secretFile && notInStore cfg.secretFile;
        message = "services.nuschl-turn.secretFile must be an absolute runtime path outside the Nix store.";
      }
      {
        assertion = lib.all (path: absolute path && notInStore path) [
          cfg.tls.certificateFile
          cfg.tls.keyFile
        ];
        message = "services.nuschl-turn.tls.certificateFile/keyFile must be absolute runtime paths outside the Nix store.";
      }
      {
        assertion = config.services.coturn.static-auth-secret == null;
        message = "Set the TURN REST secret with services.nuschl-turn.secretFile, never services.coturn.static-auth-secret (the Nix store is world-readable).";
      }
      {
        assertion = !config.services.coturn.no-auth;
        message = "services.nuschl-turn requires authenticated TURN (REST credentials).";
      }
      {
        assertion = lib.all validPeer cfg.allowedPeerAddresses;
        message = "services.nuschl-turn.allowedPeerAddresses takes single IPv4 or IPv6 addresses, not ranges or names.";
      }
      {
        assertion = cfg.allowNonPublicPeers || lib.all publicPeer cfg.allowedPeerAddresses;
        message = "services.nuschl-turn.allowedPeerAddresses contains a loopback, private, link-local, CGNAT, multicast or reserved address; set allowNonPublicPeers only for test networks.";
      }
      {
        assertion = cfg.allowedPeerPorts == null || config.networking.nftables.enable;
        message = "services.nuschl-turn.allowedPeerPorts is enforced with nftables; enable networking.nftables.";
      }
      {
        assertion = cfg.allowedPeerPorts == null || cfg.allowedPeerPorts != [ ];
        message = "services.nuschl-turn.allowedPeerPorts must list at least one port (or be null).";
      }
      {
        assertion = cfg.relayPorts.from <= cfg.relayPorts.to;
        message = "services.nuschl-turn.relayPorts.from must not exceed relayPorts.to.";
      }
      {
        assertion = !(cfg.tls.enableACME && cfg.tls.useACMEHost != null);
        message = "Choose services.nuschl-turn.tls.enableACME or tls.useACMEHost, not both.";
      }
      {
        assertion = !(useACME && useFiles);
        message = "services.nuschl-turn ACME TLS cannot be combined with certificateFile/keyFile.";
      }
      {
        assertion = (cfg.tls.certificateFile == null) == (cfg.tls.keyFile == null);
        message = "services.nuschl-turn TLS requires both certificateFile and keyFile.";
      }
      {
        assertion = !cfg.tls.nginxHTTP01 || cfg.tls.enableACME;
        message = "services.nuschl-turn.tls.nginxHTTP01 requires tls.enableACME.";
      }
    ];
    warnings =
      lib.optional (
        cfg.allowedPeerAddresses == [ ]
      ) "services.nuschl-turn has no allowedPeerAddresses: it accepts allocations but relays to no peer."
      ++
        lib.optional (cfg.allowedPeerAddresses != [ ] && cfg.allowedPeerPorts == null)
          "services.nuschl-turn.allowedPeerPorts is null: every UDP port on the SFU's addresses is reachable through the relay.";

    services.coturn = {
      enable = true;
      realm = cfg.hostName;
      use-auth-secret = true;
      static-auth-secret-file = "${credentials}/rest-secret";
      listening-port = cfg.port;
      tls-listening-port = cfg.tlsPort;
      listening-ips = cfg.listenAddresses;
      relay-ips = cfg.relayAddresses;
      min-port = cfg.relayPorts.from;
      max-port = cfg.relayPorts.to;
      no-tcp-relay = true;
      # coturn 4.17 has no CLI unless asked (`cli`) and logs no-cli as deprecated.
      no-cli = lib.versionOlder pkgs.coturn.version "4.17";
      no-tls = !useTLS;
      secure-stun = cfg.secureStun;
      cert = if useTLS then "${credentials}/tls-certificate" else null;
      pkey = if useTLS then "${credentials}/tls-key" else null;
      # Deny every peer, then allow only the SFU (coturn lets allowed-peer-ip
      # override denied-peer-ip). Loopback stays refused even when allowed:
      # allow-loopback-peers is never set. DTLS and RFC 5780 stay off (the
      # defaults), and the software version is not sent.
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
        ${lib.optionalString (cfg.bpsCapacity != null) "bps-capacity=${toString cfg.bpsCapacity}"}
        stale-nonce=600
        ${lib.concatMapStrings (address: "external-ip=${address}\n") cfg.externalAddresses}
        log-file=stdout
        simple-log
        log-min-level=${cfg.logLevel}
      '';
    };

    systemd.services.coturn = {
      wants = lib.optional useACME "acme-${acmeHost}.service";
      after = lib.optional useACME "acme-${acmeHost}.service";
      serviceConfig = {
        LoadCredential = [
          "rest-secret:${cfg.secretFile}"
        ]
        ++ lib.optionals useACME [
          "tls-certificate:${acmeDirectory}/fullchain.pem"
          "tls-key:${acmeDirectory}/key.pem"
        ]
        ++ lib.optionals (useFiles && !useACME) [
          "tls-certificate:${cfg.tls.certificateFile}"
          "tls-key:${cfg.tls.keyFile}"
        ];
        LogNamespace = namespace;
        Restart = lib.mkForce "on-failure";
        RestartSec = "5s";
        LimitNOFILE = 65536;
      };
    };
    environment.etc."systemd/journald@${namespace}.conf".text = ''
      [Journal]
      Storage=volatile
      RuntimeMaxUse=32M
      MaxRetentionSec=${cfg.logRetention}
    '';

    networking.firewall = lib.mkIf cfg.openFirewall (
      if cfg.firewallInterfaces == [ ] then
        firewallPorts
      else
        { interfaces = lib.genAttrs cfg.firewallInterfaces (_: firewallPorts); }
    );

    networking.nftables.tables.nuschl-turn-peers =
      lib.mkIf (cfg.allowedPeerPorts != null && cfg.allowedPeerAddresses != [ ])
        {
          family = "inet";
          content = ''
            chain output {
              type filter hook output priority filter; policy accept;
              ${peerPortRules "ip" peers4}
              ${peerPortRules "ip6" peers6}
            }
          '';
        };

    # coturn reads the pair once at start (LoadCredential), so a renewal restarts it.
    security.acme.certs = lib.mkIf useACME {
      ${acmeHost} = {
        reloadServices = [ "coturn.service" ];
      }
      // lib.optionalAttrs useHTTP01 {
        webroot = http01Webroot;
        dnsProvider = null;
        listenHTTP = null;
        s3Bucket = null;
        group = "nuschl-turn-acme";
      };
    };
    users.groups.nuschl-turn-acme = lib.mkIf useHTTP01 { };
    services.nginx = lib.mkIf useHTTP01 {
      enable = true;
      virtualHosts.${cfg.hostName} = {
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
        # Not nginx's enableACME: that would give nginx the key.
        locations."^~ /.well-known/acme-challenge/" = {
          root = http01Webroot;
          tryFiles = "$uri =404";
          extraConfig = ''
            auth_basic off;
            auth_request off;
            limit_except GET { deny all; }
          '';
        };
        locations."/".return = "404";
      };
    };
    systemd.services."acme-${cfg.hostName}" = lib.mkIf useHTTP01 {
      wants = [
        "nginx.service"
      ]
      ++ lib.optional config.services.nginx.enableReload "nginx-config-reload.service";
      after = [
        "nginx.service"
      ]
      ++ lib.optional config.services.nginx.enableReload "nginx-config-reload.service";
    };
  };
}
