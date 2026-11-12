# Module evaluation only: no secrets, services, network or VM are started.
let
  flake = builtins.getFlake (toString ../.);
  system = "x86_64-linux";
  pkgs = import flake.inputs.nixpkgs { inherit system; };
  lib = pkgs.lib;
  module = import ../nix/musicbot-module.nix { self = flake; };
  evaluate =
    channel:
    (flake.inputs.nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        module
        {
          system.stateVersion = "26.11";
          services.nuschl-musicbot = {
            enable = true;
            package = pkgs.writeShellScriptBin "nuschl-musicbot" "exit 0";
            credentials.acl = "/run/secrets/owned-fixture-acl";
            settings = {
              subsonic = [
                {
                  name = "fixture";
                  url = "http://127.0.0.1:4533";
                  api_key_file = "fixture";
                }
              ];
              channel = [
                (
                  {
                    name = "fixture";
                    host = "127.0.0.1";
                    username = "fixture";
                  }
                  // channel
                )
              ];
            };
          };
        }
      ];
    }).config;
  safe = evaluate { tokens_file = "acl"; };
  unsafe = evaluate { tokens = [ "public-test-secret" ]; };
  service = safe.systemd.services.nuschl-musicbot.serviceConfig;
  cases = {
    fileBackedSecret = builtins.elem "acl:/run/secrets/owned-fixture-acl" service.LoadCredential;
    inlineSecretRejected =
      !(builtins.tryEval (
        builtins.deepSeq unsafe.systemd.services.nuschl-musicbot.serviceConfig.ExecStart true
      )).success;
    noInlineSecretInSettings =
      safe.services.nuschl-musicbot.settings.channel == [
        {
          name = "fixture";
          host = "127.0.0.1";
          username = "fixture";
          tokens_file = "acl";
        }
      ];
    namespaceKinds =
      service.RestrictNamespaces == [
        "user"
        "mnt"
        "pid"
        "net"
        "ipc"
        "uts"
        "cgroup"
      ];
    limitedSyscallSetup = builtins.all (name: builtins.elem name service.SystemCallFilter) [
      "@system-service"
      "~@privileged"
      "@mount"
      "unshare"
      "setns"
      "capset"
    ];
    hardeningPreserved =
      service.DynamicUser
      && service.NoNewPrivileges
      && service.PrivateDevices
      && service.PrivateIPC
      && service.ProtectHome
      && service.MemoryDenyWriteExecute
      && service.CapabilityBoundingSet == "";
    stateOwnerOnly = service.StateDirectoryMode == "0700" && service.UMask == "0077";
    startupValidated = builtins.isString service.ExecStart;
  };
in
assert builtins.all (value: value) (builtins.attrValues cases);
cases
