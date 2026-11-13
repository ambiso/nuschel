{
  pkgs,
  nuschl,
  nativeFetchTest,
  contentWorker,
  sourceIdentity,
  fixtureTests ? ./.,
}:
let
  python = "${pkgs.python3}/bin/python3";
  fixture = "${fixtureTests}/link_previews_desktop.py";
  nativeDirectory = "/var/lib/nuschl-preview-native";
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
    systemd.services.nuschl-preview-native-peers = {
      description = "Private native preview test peers";
      serviceConfig = {
        User = "tester";
        Group = "users";
        ExecStart = "${pkgs.coreutils}/bin/env -i PATH=${pkgs.iproute2}/bin HOME=/var/empty LANG=C.UTF-8 ${python} ${fixture} native-peers --directory ${nativeDirectory}";
        WorkingDirectory = nativeDirectory;
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        NoNewPrivileges = true;
        Delegate = false;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ nativeDirectory ];
        Restart = "no";
        KillMode = "control-group";
        RuntimeMaxSec = 180;
        TimeoutStopSec = 20;
        MemoryMax = "256M";
        TasksMax = 32;
        LimitCPU = 30;
        LimitFSIZE = 1048576;
        LimitCORE = 0;
        UMask = "0077";
        StandardOutput = "append:${nativeDirectory}/peers.log";
        StandardError = "inherit";
      };
    };
    systemd.services.nuschl-preview-native-test = {
      description = "Actual native preview privacy cases";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        User = "tester";
        Group = "users";
        ExecStart = "${pkgs.coreutils}/bin/env -i PATH=${
          pkgs.lib.makeBinPath [
            pkgs.bubblewrap
            pkgs.util-linux
          ]
        } HOME=/var/empty LANG=C.UTF-8 TMPDIR=${nativeDirectory}/tmp NUSCHL_PREVIEW_FIXTURE=${nativeDirectory} NUSCHL_PREVIEW_WORKER=${contentWorker}/bin/nuschl-content-worker ${nativeFetchTest}/bin/nuschl-preview-fetch-test --ignored --exact link_preview::tests::actual_fetch_privacy --nocapture --test-threads=1";
        WorkingDirectory = nativeDirectory;
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        NoNewPrivileges = true;
        Delegate = false;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ nativeDirectory ];
        Restart = "no";
        KillMode = "control-group";
        TimeoutStartSec = 90;
        TimeoutStopSec = 20;
        MemoryMax = "1G";
        TasksMax = 64;
        LimitCPU = 30;
        LimitFSIZE = 1048576;
        LimitCORE = 0;
        UMask = "0077";
        StandardOutput = "append:${nativeDirectory}/test.log";
        StandardError = "inherit";
      };
    };
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
    import json
    import shlex
    import textwrap
    from datetime import timedelta

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
    native_directory = "${nativeDirectory}"
    machine.succeed(f"test ! -e {native_directory} && install -d -m0700 -o tester -g users {native_directory} {native_directory}/tmp")
    machine.succeed("runuser -u tester -- ${python} -c 'import json,os;open(\"${nativeDirectory}/vm-identity.json\",\"x\").write(json.dumps({\"net_inode\":os.stat(\"/proc/self/ns/net\").st_ino}))'")
    native_units = ("nuschl-preview-native-test.service", "nuschl-preview-native-peers.service")
    native_pids = {}
    native_groups = {}
    native_retirement = []
    try:
        machine.succeed("systemctl start nuschl-preview-native-peers.service")
        machine.wait_until_succeeds(f"test -f {native_directory}/peers-ready.json", timeout=timedelta(seconds=15))
        peer_group = machine.succeed("systemctl show -p ControlGroup --value nuschl-preview-native-peers.service").strip()
        assert peer_group == "/system.slice/nuschl-preview-native-peers.service", peer_group
        native_groups[native_units[1]] = peer_group
        peer_ready = json.loads(machine.succeed(f"test $(stat -c%s {native_directory}/peers-ready.json) -le 32768 && cat {native_directory}/peers-ready.json"))
        assert type(peer_ready["pid"]) is int and peer_ready["pid"] > 0
        assert machine.succeed("systemctl show -p ExecMainPID --value nuschl-preview-native-peers.service").strip() == str(peer_ready["pid"])
        native_pids[native_units[1]] = peer_ready["pid"]
        native_status, native_log = machine.execute("systemctl start nuschl-preview-native-test.service", timeout=timedelta(seconds=120))
        assert len(native_log) <= 16384, "bounded native launcher output exceeded"
        machine.succeed("printf %s " + shlex.quote(native_log) + f" > {native_directory}/launcher.log")
        test_pid = machine.succeed("systemctl show -p ExecMainPID --value nuschl-preview-native-test.service").strip()
        assert test_pid.isdecimal() and int(test_pid) > 0, test_pid
        native_pids[native_units[0]] = int(test_pid)
        assert native_status == 0, native_log
        authority = json.loads(machine.succeed(f"test $(stat -c%s {native_directory}/test-process-authority.json) -le 16384 && cat {native_directory}/test-process-authority.json"))
        assert authority["cgroup"] == "0::/system.slice/nuschl-preview-native-test.service\n", authority
        native_groups[native_units[0]] = authority["cgroup"].split("::", 1)[1].rstrip("\n")
        result = json.loads(machine.succeed(f"test $(stat -c%s {native_directory}/fetch-result.json) -le 65536 && cat {native_directory}/fetch-result.json"))
        assert result["passed"] is True and len(result["checks"]) == 13, result
        # The test requests peer shutdown. Let its joined finalizer publish the
        # final event oracle before finally requests forced cleanup.
        machine.wait_until_succeeds("test \"$(systemctl show -p ActiveState --value nuschl-preview-native-peers.service)\" = inactive", timeout=timedelta(seconds=15))
        peer_exit = {name: machine.succeed(f"systemctl show -p {name} --value nuschl-preview-native-peers.service").strip()
            for name in ("ActiveState", "Result", "ExecMainStatus")}
        assert peer_exit == {"ActiveState": "inactive", "Result": "success", "ExecMainStatus": "0"}, peer_exit
        machine.succeed(f"test -f {native_directory}/peers.json")
        machine.succeed("printf %s " + shlex.quote(json.dumps(peer_exit)) + f" > {native_directory}/peer-exit.json")
    finally:
        # Requesting stop is insufficient: admission below also requires the
        # original nondelegated kernel cgroups empty/removed and leaders gone.
        stop_status, stop_log = machine.execute("systemctl stop " + " ".join(native_units), timeout=timedelta(seconds=60))
        assert len(stop_log) <= 16384, "bounded native stop output exceeded"
        machine.succeed("printf %s " + shlex.quote(stop_log) + f" > {native_directory}/stop.log")
        machine.succeed(f"sha256sum ${nativeFetchTest}/bin/nuschl-preview-fetch-test ${contentWorker}/bin/nuschl-content-worker ${fixture} ${fixtureTests}/link_preview_fetch.py > {native_directory}/artifacts.sha256")
        machine.succeed("printf %s " + shlex.quote("${sourceIdentity}") + f" > {native_directory}/source-identity.txt")
        machine.succeed(f"(ip -4 route show table all; ip -6 route show table all; iptables-save; ip6tables-save) > {native_directory}/routes.txt")
        machine.succeed("install -d /tmp/preview-native-evidence")
        for name in ("launcher.log", "stop.log", "test.log", "peers.log", "fetch-result.json", "test-process-authority.json", "peers-ready.json", "peers.json", "peer-exit.json", "artifacts.sha256", "source-identity.txt", "routes.txt"):
            machine.succeed(f"if test -f {native_directory}/{name}; then test $(stat -c%s {native_directory}/{name}) -le 1048576 && cp {native_directory}/{name} /tmp/preview-native-evidence/; fi")
        machine.copy_from_machine("/tmp/preview-native-evidence", "native")
        assert stop_status == 0, stop_log
        for unit in native_units:
            group = "/sys/fs/cgroup/system.slice/" + unit
            pid = native_pids.get(unit)
            retirement_check = f"""
    from pathlib import Path
    Path('/proc/self/stat').read_text()
    Path('/sys/fs/cgroup/cgroup.controllers').read_text()
    group = Path({group!r})
    try:
        group.stat()
    except FileNotFoundError:
        pass
    else:
        assert 'populated 0' in (group / 'cgroup.events').read_text().splitlines()
    if {pid is not None!r}:
        try:
            Path('/proc/{pid}').stat()
        except FileNotFoundError:
            pass
        else:
            raise AssertionError('original service leader still exists')
    """
            machine.wait_until_succeeds("${python} -c " + shlex.quote(textwrap.dedent(retirement_check)), timeout=timedelta(seconds=15))
            native_retirement.append({"unit": unit, "original_group": native_groups.get(unit), "kernel_group": group, "leader_pid": pid, "kernel_empty_or_removed": True, "leader_absent": pid is not None})
        machine.succeed("printf %s " + shlex.quote(json.dumps(native_retirement)) + f" > {native_directory}/retirement.json")
        machine.copy_from_machine(f"{native_directory}/retirement.json", "native-retirement.json")
    assert all(item["leader_absent"] for item in native_retirement), native_retirement
    native_peers = json.loads(machine.succeed(f"test $(stat -c%s {native_directory}/peers.json) -le 524288 && cat {native_directory}/peers.json"))
    assert native_peers["overrun"] is False
    assert isinstance(native_peers["events"], list) and len(native_peers["events"]) <= 4096
    assert not any(event["kind"] == "peer_error" and event.get("error") == "AssertionError" for event in native_peers["events"]), native_peers
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
