# Disposable Windows VM harness

`tests/windows_vm.py` drives a disposable Windows 11 guest with QMP and a
PowerShell agent. It uses a synthetic audio backend, never host audio devices.
QEMU user networking remains `restrict=on`; only explicit guest forwards reach
host loopback. Do not attach personal profiles, identities or private files.

Run host boundary tests without Windows or Nix:

```sh
TMPDIR="$HOME/.cache/nuschl-next/tmp" python3 -m unittest discover -s tests -p windows_vm_test.py -v
```

State defaults to `~/vms/nuschl-windows`; `NUSCHL_WINDOWS_VM_DIR` can select an
existing, older `~/vms/nuschel-windows` installation in place. Shut down any owner
of that image first. `up` takes an exclusive persistent daemon lock and makes
state owner-only. It never deletes an existing disk to reinstall Windows. If an
existing disk lacks its `installed` marker, inspect it and restore the marker
only after confirming installation completed. Installation of a new guest still
requires a Windows installer ISO; normal boots do not attach that installer.

Every `up` generates a fresh 256-bit bearer credential and rebuilds the private,
read-only `NUSCHLVM` seed ISO. The token never appears in URLs, QMP commands,
process arguments or request logs. Jobs, results and file downloads all require
that token. Control requests additionally pin the current agent script revision.
The agent reads credentials from current media and disables proxies and redirects.
An older installed guest is upgraded by a bounded series of QMP Run-dialog
commands invoking `firstlogon.ps1 -RefreshOnly` from that media. This replaces
only the old/new harness agent and startup entry. Leave the desktop idle until
`wait-agent` succeeds; automatic refresh depends on the disposable auto-login
session and its US keyboard layout. A locked/logged-out/customized guest may
need manual login and invocation of that same refresh script.

The harness restricts HTTP headers, body and output sizes, concurrent connections
and outstanding jobs. Invalid requests cannot dequeue jobs, publish results,
mark installation complete or serve files. Results are accepted once, only for
issued jobs. Timed-out jobs are retired; jobs are deliberately not retried because
arbitrary scripts may have effects. A disconnected agent can therefore require a
new explicit test run. Offered files use flat ASCII names; symlinks, directories,
FIFOs and path traversal are rejected.

To offer a fixture, use `windows_vm.py push FILE [NAME]`, then run:

```powershell
Get-NuschlTestFile -Name fixture.txt -OutFile "$env:TEMP\fixture.txt"
```

This helper supplies authentication only to the fixed harness endpoint. Raw
`Invoke-WebRequest http://10.0.2.100/files/...` calls now receive HTTP 401.
Migrate old update/confinement fixture downloads to the helper; raw TCP probes
against an explicitly forwarded test service retain their existing meaning.
`tests/windows_update.py` authenticates and prefetches its synthetic signed
artifacts, then serves that dedicated directory on the **guest's own loopback**
for the production updater. No harness token enters updater configuration. The
fixture server is stopped after the test, including assertion failures.

The trust boundary is host versus other local users and accidental/uncredentialed
HTTP callers, not the disposable guest's administrator or host root/same-user
processes. HTTP stays inside the private QEMU/host-loopback path; this is not a
public remote management service. The guest can execute trusted test scripts and
read explicitly offered fixtures. Never use this harness for an untrusted,
multi-user or production VM.
