# SPDX-License-Identifier: EUPL-1.2
"""NixOS test-driver script, following the shared pre-boot disk-path guard."""
import json
import os
from pathlib import Path
import shlex

start_all()  # noqa: F821
reports = []
for machine in machines:  # noqa: F821
    machine.wait_for_unit("dnsmasq.service")
    machine.wait_for_unit("murmur.service")
    machine.wait_for_unit("nuschl-web-relay.service")
    machine.wait_for_unit("nginx.service")
    machine.wait_for_open_port(80)
    machine.fail("systemctl is-active nscd.service")
    nss = machine.succeed("grep '^hosts:' /etc/nsswitch.conf")
    assert nss.split()[1:] == ["files", "dns"], nss
    expected_resolver = "127.0.0.53" if machine.name == "resolved" else "127.0.0.55"
    resolv = machine.succeed("cat /etc/resolv.conf")
    assert f"nameserver {expected_resolver}" in resolv, resolv
    machine.fail("grep -q voice.dns.test /etc/hosts")
    route = machine.succeed("ip -4 route")
    assert "default " not in route, route
    initial = json.loads(machine.succeed("cat /run/nuschl-web-relay/dns-filter-proof.json"))
    # These endpoints really listen outside the restricted cgroup: a refused
    # connection alone must not be mistaken for proof of the kernel filter.
    machine.succeed("python3 -c 'import socket; [socket.create_connection((ip,64738),timeout=2).close() for ip in [\"127.0.0.1\",\"127.0.0.56\"]]'")
    assert initial["resolver"] == expected_resolver, initial
    assert "nuschl-web-relay.service" in initial["cgroup"], initial
    assert initial["other_loopbacks_blocked"] == ["127.0.0.1", "127.0.0.56"]
    assert initial["dns_address_other_port_reachable"], initial
    base = f"nuschl-web-relay-probe --relay ws://127.0.0.1/relay --origin http://{machine.name} --server "
    connected = machine.succeed(base + "voice.dns.test:64738")
    assert "connected session=" in connected and "udp_answered=true" in connected, connected
    # A resolver exception permits kernel traffic to another port, but never
    # grants the application a private-address tunnel allowance.
    for target in ["resolver.dns.test:64738", expected_resolver + ":64738", "127.0.0.1:64738", "denied.dns.test:64738"]:
        refused = machine.fail(base + shlex.quote(target))
        assert "AddressRefused" in refused, (target, refused)
    denied = machine.succeed("systemctl show nuschl-web-relay.service -p IPAddressDeny")
    assert "127.0.0.0/8" in denied and "10.0.0.0/8" in denied, denied
    accounting = {}
    for field in ["IPIngressBytes", "IPEgressBytes"]:
        accounting[field] = int(machine.succeed(f"systemctl show nuschl-web-relay.service --value -p {field}").strip())
        assert 0 < accounting[field] < 2**63, accounting
    journal = machine.succeed("journalctl -u nuschl-web-relay.service --no-pager")
    assert "Proceeding WITHOUT firewalling in effect" not in journal, journal
    # Remove ONLY the DNS address from this owned guest's runtime test drop-in.
    # Forced libc NSS files+dns avoids Unix nss-resolve bypassing the IP filter.
    machine.succeed("mkdir -p /run/systemd/system/nuschl-web-relay.service.d")
    machine.succeed("printf '[Service]\\nIPAddressAllow=\\nIPAddressAllow=192.168.1.0/24\\n' > /run/systemd/system/nuschl-web-relay.service.d/dns-negative.conf")
    machine.succeed("systemctl daemon-reload")
    machine.fail("systemctl restart nuschl-web-relay.service")
    failure = machine.succeed("journalctl -u nuschl-web-relay.service --no-pager -n 25")
    assert "gaierror" in failure, failure
    machine.succeed("rm /run/systemd/system/nuschl-web-relay.service.d/dns-negative.conf")
    machine.succeed("systemctl daemon-reload")
    machine.succeed("systemctl reset-failed nuschl-web-relay.service")
    machine.succeed("systemctl start nuschl-web-relay.service")
    machine.wait_for_unit("nuschl-web-relay.service")
    recovered = machine.succeed(base + "voice.dns.test:64738")
    assert "connected session=" in recovered, recovered
    reports.append({"machine":machine.name,"resolver_config":resolv,"route":route,"cgroup_probe":initial,
                    "BPF_IPAccounting":accounting,
                    "DNS_dependent_private_Murmur":True,"resolver_tunnel_refused":True,"explicit_deny_overrides_LAN_allow":True,
                    "without_DNS_exception_fails":True,"restored_original_module_recovers":True})
Path(os.environ["NUSCHL_VM_SCRATCH_ROOT"], "dns-proof.json").write_text(json.dumps({
    "passed":True,"cases":reports,"scope":"actual NixOS unit/cgroup +owned authoritative test DNS/Murmur only; no public resolver/server",
},indent=2)+"\n")
