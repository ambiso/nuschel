# Standalone, private systemd lifecycle proof; no public ACME issuer is contacted.
# Build only `.driver`, then execute it with a disk-backed TMPDIR/output directory.
{
  projectRoot ? ../.,
  packagePath ? "/nix/store/n3cw1mrbi7c7wzb1ry0qhd70vw18r2ik-nuschl-0.1.0",
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
      # All fixture authority material is generated inside the guest. This test
      # uses the normal publisher/verifier, with its CA bundle selected at runtime.
      security.pki.caBundle = lib.mkForce "/run/pki-fixture/root.pem";
      security.acme = {
        acceptTerms = true;
        preliminarySelfsigned = false;
        defaults = {
          email = "fixture@example.test";
          webroot = "/var/lib/acme/challenge";
        };
      };
      # Keep the actual NixOS ACME target, post-run hook and unit sandbox. Replace
      # only the issuer command with an offline marker; it never invokes lego.
      systemd.services."acme-voice.test".script = lib.mkForce ''
        touch /tmp/out/renewed
      '';
      systemd.services.nuschl.wantedBy = lib.mkForce [ ];
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
    start_all()
    server.wait_for_unit("multi-user.target")
    server.succeed("true")
  '';
}
