# Standalone, private systemd lifecycle proof; no public ACME issuer is contacted.
# Build only `.driver`, then execute it with a disk-backed TMPDIR/output directory.
{
  projectRoot ? ../.,
  packagePath ? "/nix/store/n3cw1mrbi7c7wzb1ry0qhd70vw18r2ik-nuschl-0.1.0",
  trustedCaFile ? null,
}:
let
  flake = builtins.getFlake (toString projectRoot);
  pkgs = flake.inputs.nixpkgs.legacyPackages.x86_64-linux;
  lib = pkgs.lib;
  # Reuse an already-built package, including its complete runtime closure.
  # This tiny wrapper cannot request a Cargo build.
  package = pkgs.runCommand "nuschl-cached-pki-vm-package" { } ''
    ln -s ${builtins.storePath packagePath} "$out"
  '';
in
pkgs.testers.runNixOSTest {
  name = "nuschl-pki-lifecycle";
  nodes.server =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    {
      imports = [ (import ../nix/module.nix { self = flake; }) ];
      services.nuschl = {
        enable = true;
        inherit package;
        hostName = "voice.test";
        tls.enableACME = true;
        listenAddresses = [ "127.0.0.1" ];
        kt = {
          defaultWitnesses = false;
          origins = [ ];
        };
        settings.autoban.attempts = 0;
      };
      # Only a PUBLIC fixture root enters the store. Its private key is generated
      # in owned disk scratch and copied into the guest at runtime by the driver.
      security.pki.certificateFiles = lib.optional (trustedCaFile != null) trustedCaFile;
      security.acme = {
        acceptTerms = true;
        defaults = {
          email = "fixture@example.test";
          webroot = "/var/lib/acme/challenge";
        };
      };
      # Keep the actual NixOS ACME target, post-run hook and unit sandbox. Replace
      # only the issuer command with an offline marker; it never invokes lego.
      systemd.services."acme-order-renew-voice.test".script = lib.mkForce ''
        touch /tmp/out/renewed
      '';
      systemd.services.nuschl.wantedBy = lib.mkForce [ ];
      # Keep the credential mount alive long enough to inspect access under
      # the actual helper/server UIDs. The publisher itself is unchanged.
      systemd.services.nuschl-tls-refresh.serviceConfig.ExecStartPre = "${pkgs.coreutils}/bin/sleep 1";
      systemd.timers.nuschl-tls-refresh.timerConfig = {
        OnBootSec = lib.mkForce "2s";
        OnUnitInactiveSec = lib.mkForce "5s";
      };
      environment.systemPackages = [
        pkgs.openssl
        pkgs.python3
        pkgs.util-linux
        pkgs.procps
      ];
      environment.etc."pki-fixture-hook".text = config.security.acme.certs."voice.test".postRun;
      virtualisation = {
        memorySize = 1024;
        cores = 2;
        diskSize = 1024;
        vlans = [ ];
      };
    };
  testScript = ''
    import os
    from pathlib import Path

    fixture = Path(os.environ["NUSCHL_PKI_FIXTURE_DIRECTORY"])
    start_all()
    server.wait_for_unit("multi-user.target")
    server.succeed("install -d -m0700 /root/pki-fixture")
    for name in ("root.pem", "root.key"):
        server.copy_from_host(str(fixture / name), "/root/pki-fixture/" + name)
    try:
        server.succeed("python3 ${./server_pki_vm.py} guest", timeout=240)
    finally:
        server.succeed("journalctl -u nuschl -u nuschl-tls-refresh --no-pager > /root/pki-fixture/journal.txt")
        server.copy_from_machine("/root/pki-fixture/journal.txt")
        server.succeed("test ! -f /root/pki-fixture/proof.json || cp /root/pki-fixture/proof.json /tmp/pki-proof.json")
        if server.succeed("test -f /tmp/pki-proof.json && echo yes || true").strip() == "yes":
            server.copy_from_machine("/tmp/pki-proof.json")
  '';
}
