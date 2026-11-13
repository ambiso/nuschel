{
  pkgs,
  nuschl,
  sourceIdentity,
  fixtureTests ? ./.,
}:
let
  python = "${pkgs.python3}/bin/python3";
  fixture = "${fixtureTests}/link_previews_desktop.py";
  setupPeers = pkgs.writeShellScript "nuschl-preview-test-peers-setup" ''
    set -eu
    test -f /run/nuschl-preview-dedicated-vm
    install -d -m0700 /var/lib/nuschl-preview-peers
    cd /var/lib/nuschl-preview-peers
    test ! -e ca-key.pem
    umask 077
    ${pkgs.openssl}/bin/openssl req -x509 -newkey rsa:2048 -nodes \
      -subj '/CN=Owned preview test CA' -days 1 \
      -addext 'basicConstraints=critical,CA:TRUE' \
      -addext 'keyUsage=critical,keyCertSign,cRLSign' \
      -keyout ca-key.pem -out ca.pem >/dev/null 2>&1
    ${pkgs.openssl}/bin/openssl req -newkey rsa:2048 -nodes \
      -subj '/CN=resolver.preview.example' -keyout server-key.pem \
      -out server.csr >/dev/null 2>&1
    cat > extensions <<'EOF'
    basicConstraints=critical,CA:FALSE
    keyUsage=critical,digitalSignature,keyEncipherment
    extendedKeyUsage=serverAuth
    subjectAltName=DNS:resolver.preview.example
    EOF
    ${pkgs.openssl}/bin/openssl x509 -req -in server.csr -CA ca.pem \
      -CAkey ca-key.pem -CAcreateserial -days 1 -extfile extensions \
      -out server.pem >/dev/null 2>&1
    ${python} -c 'import json,os;open("vm-identity.json","w").write(json.dumps({"net_inode":os.stat("/proc/self/ns/net").st_ino}))'
  '';
in
pkgs.testers.runNixOSTest {
  name = "nuschl-link-previews-desktop";
  nodes.machine = {
    # If production accidentally performs system DNS, the owned peer records
    # it; no resolver request can escape this VM's loopback-only output policy.
    networking.nameservers = [ "127.0.0.53" ];
    services.resolved.enable = false;
    environment.systemPackages = [
      nuschl
      pkgs.python3
      pkgs.gnome-shell
      pkgs.at-spi2-core
      pkgs.dbus
      pkgs.glib
      pkgs.bash
      pkgs.pulseaudio
      pkgs.murmur
      pkgs.openssl
      pkgs.imagemagick
      pkgs.iproute2
      pkgs.iptables
      pkgs.procps
      pkgs.bubblewrap
      pkgs.util-linux
    ];
    hardware.graphics.enable = true;
    fonts.packages = [ pkgs.dejavu_fonts ];
    services.dbus.enable = true;
    users.users.tester = {
      isNormalUser = true;
      createHome = true;
    };
    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
    # No passthrough or virgl option: only the VM's software graphics device.
    systemd.services.nuschl-preview-peers = {
      description = "Synthetic preview peers inside the isolated test VM";
      path = [ pkgs.iproute2 ];
      serviceConfig = {
        Type = "simple";
        ExecStartPre = setupPeers;
        ExecStart = "${python} ${fixture} peers --directory /var/lib/nuschl-preview-peers";
        RuntimeMaxSec = 750;
        TimeoutStopSec = 35;
        KillMode = "control-group";
        Restart = "no";
        UMask = "0077";
      };
    };
  };
  testScript = ''
    import shlex

    start_all()
    machine.wait_for_unit("multi-user.target")
    # These commands affect ONLY the dedicated guest. No synthetic public
    # address can reach the Internet, even if QEMU supplied an extra NIC.
    machine.succeed("ip -4 route flush default; ip -6 route flush default")
    machine.succeed("iptables -F OUTPUT; iptables -P OUTPUT DROP; iptables -A OUTPUT -o lo -j ACCEPT")
    machine.succeed("ip6tables -F OUTPUT; ip6tables -P OUTPUT DROP; ip6tables -A OUTPUT -o lo -j ACCEPT")
    for address in ("93.184.215.14", "93.184.215.53", "93.184.215.99"):
        machine.succeed(f"ip addr add {address}/32 dev lo")
    machine.succeed("touch /run/nuschl-preview-dedicated-vm")
    machine.succeed("systemctl start nuschl-preview-peers.service")
    machine.wait_until_succeeds("test -f /var/lib/nuschl-preview-peers/ready.json")
    machine.succeed("install -d -m0700 -o tester -g users /home/tester/.cache/lp /home/tester/.cache/lp/tmp /home/tester/.cache/lp/wl")
    failures = []
    try:
        for server in ("stock", "nuschld"):
            output = "/home/tester/.cache/lp/" + server
            args = [
                "${python}", "${fixture}", "desktop",
                "--binaries", "${nuschl}/bin",
                "--murmur", "${pkgs.murmur}/bin/mumble-server",
                "--at-spi", "${pkgs.at-spi2-core}",
                "--gnome-shell", "${pkgs.gnome-shell}/bin/gnome-shell",
                "--pulse-bin", "${pkgs.pulseaudio}/bin",
                "--output", output, "--server", server,
                "--source-identity", "${sourceIdentity}",
            ]
            command = "runuser -u tester -- env -i " + shlex.join([
                "HOME=/home/tester", "USER=tester", "LOGNAME=tester",
                "PATH=/run/current-system/sw/bin",
                "TMPDIR=/home/tester/.cache/lp/tmp",
                "NUSCHL_WL_TMP=/home/tester/.cache/lp/wl",
                "PULSE_SERVER=unix:/home/tester/.cache/lp/no-host-pulse",
                "PULSE_RUNTIME_PATH=/home/tester/.cache/lp/no-host-pulse-runtime",
                "PIPEWIRE_REMOTE=/home/tester/.cache/lp/no-host-pipewire",
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/home/tester/.cache/lp/no-host-session-bus",
                "GSETTINGS_BACKEND=memory", "LIBGL_ALWAYS_SOFTWARE=1",
                "MAGICK=${pkgs.imagemagick}/bin/magick",
            ] + args)
            status, scenario_log = machine.execute(command, timeout=300)
            machine.succeed("install -d /tmp/preview-evidence/" + server)
            # Never export the retained profile, test TLS private keys or CA key.
            machine.execute("find " + shlex.quote(output) + " -maxdepth 1 -type f "
                + "\\( -name '*.json' -o -name '*.png' -o -name '*.log' \\) "
                + "-exec cp '{}' /tmp/preview-evidence/" + server + "/ \\;")
            assert len(scenario_log) <= 1024 * 1024, "bounded scenario stdout exceeded 1MiB"
            machine.succeed("printf %s " + shlex.quote(scenario_log) + " > /tmp/preview-evidence/" + server + "/scenario.log")
            machine.copy_from_machine("/tmp/preview-evidence/" + server, server)
            if status != 0:
                failures.append((server, status, scenario_log[-8000:]))
    finally:
        machine.execute("${python} -c 'import sys;sys.path.insert(0,\"${fixtureTests}\");from link_previews_desktop import control;control(\"stop\")'")
        machine.wait_until_fails("systemctl is-active --quiet nuschl-preview-peers.service")
        machine.copy_from_machine("/var/lib/nuschl-preview-peers/peers.json", "peers.json")
        machine.succeed("(ip -4 route show table all; ip -6 route show table all; iptables-save; ip6tables-save) > /tmp/network-proof.txt")
        machine.copy_from_machine("/tmp/network-proof.txt", "network-proof.txt")
    assert not failures, failures
  '';
}
