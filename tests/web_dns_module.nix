# SPDX-License-Identifier: EUPL-1.2
# Real NixOS module evaluation; no package build, resolver or network access.
let
  source = builtins.getFlake ("path:" + toString ../.);
  lib = source.inputs.nixpkgs.lib;
  pkgs = import source.inputs.nixpkgs { system = "x86_64-linux"; };
  module = import ../nix/web-module.nix { self = source; };
  evaluate =
    extra:
    let
      machine = lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          module
          {
            system.stateVersion = "26.05";
            services.nuschl-web = {
              enable = true;
              hostName = "web.test";
              enableACME = false;
              forceSSL = false;
              package = pkgs.emptyDirectory;
              relay.package = pkgs.emptyDirectory;
            };
          }
          extra
        ];
      };
      cfg = machine.config.services.nuschl-web;
      service = machine.config.systemd.services.nuschl-web-relay;
    in
    {
      resolved = machine.config.services.resolved.enable;
      resolvers = cfg.relay.resolverAddresses;
      allow = service.serviceConfig.IPAddressAllow;
      deny = service.serviceConfig.IPAddressDeny;
      allowNetworks = cfg.relay.allowNetworks;
      # Identical content-addressed relay config proves resolver exceptions do
      # not leak into its application destination allowance (without IFD).
      relayConfig = toString (builtins.head service.restartTriggers);
      inheritedHardening = {
        inherit (service.serviceConfig)
          NoNewPrivileges
          PrivateDevices
          ProtectHome
          RestrictNamespaces
          ;
      };
    };
  withoutResolved = evaluate { services.resolved.enable = false; };
  withResolved = evaluate { services.resolved.enable = true; };
  custom = evaluate {
    services.resolved.enable = true;
    services.nuschl-web.relay.resolverAddresses = [
      "127.0.0.55"
      "fd12:3456::53"
    ];
  };
  disabledDefault = evaluate {
    services.resolved.enable = true;
    services.nuschl-web.relay.resolverAddresses = [ ];
  };
  lan = evaluate {
    services.resolved.enable = false;
    services.nuschl-web.relay = {
      resolverAddresses = [
        "127.0.0.55"
        "127.0.0.55"
      ];
      allowNetworks = [
        "192.168.2.0/24"
        "127.0.0.55"
      ];
    };
  };
  denySetting = evaluate {
    services.resolved.enable = true;
    services.nuschl-web.relay.settings.deny_networks = [ "198.51.100.0/24" ];
  };
  baselineDeny = [
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
assert withoutResolved.resolvers == [ ];
assert withoutResolved.allow == [ ];
assert withResolved.resolvers == [ "127.0.0.53" ];
assert withResolved.allow == [ "127.0.0.53" ];
assert
  custom.resolvers == [
    "127.0.0.55"
    "fd12:3456::53"
  ];
assert custom.allow == custom.resolvers;
assert disabledDefault.allow == [ ];
assert
  lan.allow == [
    "192.168.2.0/24"
    "127.0.0.55"
  ];
assert builtins.all (entry: entry.deny == baselineDeny) [
  withoutResolved
  withResolved
  custom
  disabledDefault
  lan
  denySetting
];
assert withResolved.relayConfig == withoutResolved.relayConfig;
assert custom.relayConfig == withResolved.relayConfig;
assert disabledDefault.relayConfig == withResolved.relayConfig;
assert lan.relayConfig != withResolved.relayConfig;
assert denySetting.relayConfig != withResolved.relayConfig;
assert builtins.all (entry: entry.inheritedHardening == withoutResolved.inheritedHardening) [
  withResolved
  custom
  disabledDefault
  lan
  denySetting
];
{
  passed = true;
  cases = {
    inherit
      withoutResolved
      withResolved
      custom
      disabledDefault
      lan
      denySetting
      ;
  };
  checks = [
    "resolved-off has no default IP exception"
    "resolved-on has only its dedicated127.0.0.53 stub"
    "custom addresses replace rather than append resolved default"
    "explicit empty list disables default"
    "allowNetworks composes and deduplicates without widening CIDRs"
    "kernel deny snapshot and other hardening are unchanged"
    "DNS-only changes leave application relay config identical"
    "explicit allowNetworks and deny_networks settings still affect relay config"
  ];
}
