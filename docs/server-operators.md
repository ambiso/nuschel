# Running a Nuschl server on NixOS

This guide covers the voice/chat server, optional browser client and relay, and
optional key-transparency witness. It reviews configuration, not the state of a
particular deployment. No public Mumble endpoint was contacted for this review.
Keep your flake inputs locked and review the selected revision before deploying.

## Review of the supplied configuration

The supplied `nuschl.eu` configuration connects the services correctly:

| Setting | Result |
| --- | --- |
| `services.nuschl-web.enable = true; hostName = "nuschl.eu";` | Enables nginx, HTTPS/ACME by default, static `/app/` and `/join/`, and the `/relay` WebSocket tunnel |
| nginx `locations."= /".return = "302 /app/"` | Redirects only the exact root URL; it preserves `/relay`, share links and app assets |
| `services.nuschl.hostName = "nuschl.eu"; tls.useACMEHost = "nuschl.eu";` | Selects the web host's ACME **full chain and key** for native TLS on port 64738; nginx HTTPS alone would not select them for the voice server |
| `superUserPasswordFile = config.age.secrets.nuschl-superuser-password.path` | PID 1 loads the runtime secret through `LoadCredential`; the password replaces the stored SuperUser password on each server start |
| `kt.origins = [ "nuschl.eu:64738" ];` | Matches clients connecting with that lowercase DNS name and port; this is distinct from the witness URL or web origin |
| witness URL `https://witness.nuschl.eu` and verifier-key file | Configures server submissions, provided that the public key is the actual running witness's key with its exact cosigner name |
| `settings.share_links.web_host = "nuschl.eu"` | Generates links for the web host; it does not itself create DNS, certificates, firewall rules or a relay |
| `security = "compatible"` | Allows stock Mumble and normal voice, with TLS transport but **no end-to-end encryption** |

These dependencies are not established by the excerpt; they may already be
configured elsewhere in the host's configuration:

- Import the server, web and witness NixOS modules, and the agenix module when
  using `age.secrets`. The encrypted `.age` file and public `.vkey` file must be
  present in the flake's tracked inputs. Never include decrypted passwords or
  private keys in Nix strings or the store.
- Set global `security.acme.acceptTerms` and an ACME account/contact email, and
  provide a working challenge. Both nginx virtual hosts select the normal HTTP
  challenge webroot by default. A separately configured DNS challenge is another
  option; it needs its own runtime credentials.
- Publish correct public A and, if used, AAAA records for both hostnames. They
  must reach this server, including from its relay/witness. Do not publish a stale
  IPv6 record or resolve the public names to loopback/private/Tailscale addresses
  just to reach a cohosted service.
- Permit inbound **TCP 80 and 443** for web, witness HTTPS and HTTP ACME challenges,
  including any provider firewall/NAT. The excerpt opens only TCP/UDP 64738 on
  `ens3` and `tailscale0`. The web module does not open 80/443; witness
  `openFirewall` defaults to false. An existing global nginx/firewall rule may
  already supply them. With DNS challenges, HTTP 80 is unnecessary for issuance
  but remains necessary if you want HTTP-to-HTTPS redirects.
- Check outbound DNS, HTTPS and connections to the server port. The relay must
  resolve and reach public destinations; the witness must reach the configured
  public KT origin for enrollment. Local networking/routing still matters when
  all three services run on one machine.

`lib.removeSuffix "\n" (builtins.readFile ./nuschl-witness.vkey)` is appropriate
for the single newline emitted by the witness tool. The file is public verifier
material, not a private signing seed. Do not substitute a TLS certificate or a
server KT key. Nix evaluation cannot authenticate that the file names the running
witness; verify it as described below.

## Minimal public voice/chat and web setup

Add these modules to your existing host's `nixosSystem.modules` list:

```nix
inputs.nuschl.nixosModules.default
inputs.nuschl.nixosModules.web
inputs.agenix.nixosModules.default  # if not already imported
```

The input may be named differently in your flake; use that name consistently.
The following host module gives a complete service configuration once you supply
real DNS, your existing host/hardware configuration, and the encrypted secret.
Replace the hostname, interface, email and secret-file path:

```nix
{ config, ... }:
let
  host = "voice.example.org";
in {
  age.secrets.nuschl-superuser-password.file = ./secrets/nuschl-superuser-password.age;

  security.acme = {
    acceptTerms = true; # operator's ACME terms decision
    defaults.email = "operator@example.org";
  };

  services.nuschl-web = {
    enable = true;
    hostName = host;
    # enableACME, forceSSL and relay.enable default to true.
  };
  services.nginx.virtualHosts.${host}.locations."= /".return = "302 /app/";

  services.nuschl = {
    enable = true;
    hostName = host;
    security = "compatible";
    tls.useACMEHost = host;
    superUserPasswordFile = config.age.secrets.nuschl-superuser-password.path;
    kt.origins = [ "${host}:64738" ];
    settings.share_links.web_host = host;
  };

  networking.firewall.interfaces.ens3 = {
    allowedTCPPorts = [ 80 443 64738 ];
    allowedUDPPorts = [ 64738 ];
  };
}
```

This preserves interface-scoped exposure rather than opening every interface.
Your Tailscale-only 64738 rules can remain separate. The server defaults to
listening on IPv4/IPv6 wildcard addresses, port 64738; the firewall controls
reachability. Native clients use that port, not nginx's HTTPS port.

The secret manager must install its runtime file before the service starts.
Keep it private; PID 1 can read a root-owned secret for `LoadCredential`, so the
dynamic server user does not need broad access to agenix files. Rotate the secret
and restart `nuschl.service` to apply a new SuperUser password. `passwordFile` is
a different setting: the server-access password, not the administrator password.
Removing `superUserPasswordFile` does not erase a previously stored password.

Without a browser client, omit the web module and configure server TLS explicitly
using the [server module's ACME/manual alternatives](server.md#reusing-an-acme-certificate).
A self-signed server is supported, but clients must explicitly trust/pin it.

## Separate voice hostname with HTTP-01 only

For a fresh setup with web on `nuschl.eu`, voice on `voice.nuschl.eu`, and the
optional witness unchanged on `witness.nuschl.eu`, use:

```nix
services.nuschl = {
  enable = true;
  hostName = "voice.nuschl.eu";
  security = "compatible";
  tls = {
    enableACME = true;
    nginxHTTP01 = true;
  };
  superUserPasswordFile = config.age.secrets.nuschl-superuser-password.path;
  kt.origins = [ "voice.nuschl.eu:64738" ];
  settings.share_links.web_host = "nuschl.eu";
};
```

Keep the existing web/witness service definitions, global ACME account settings
and interface firewall rules described above. Publish the voice hostname's DNS
records and make TCP 80 reachable for HTTP-01. Native clients connect to
`voice.nuschl.eu:64738`. The explicit KT origin avoids the default, which derives
from the machine's FQDN rather than `services.nuschl.hostName`.

`tls.nginxHTTP01` defaults to **false**. It requires `tls.enableACME` and a concrete
`hostName`; use it instead of `tls.useACMEHost`. It enables nginx and supplies
exactly an HTTP port-80 virtual host for that voice name. The challenge prefix
`/.well-known/acme-challenge/` serves token GETs from a dedicated public webroot;
other HTTP paths return 404. It creates **no HTTPS voice virtual host**, redirect,
web app or nginx certificate/key selection. Web and witness HTTPS hosts remain
separate. On a shared IP, port 443 still serves those other hosts; this is not a
claim that a TCP connection to that IP's port 443 becomes impossible.

The voice certificate is its own `security.acme.certs."voice.nuschl.eu"` entry.
Its private `nuschl-acme` group is not granted to nginx or the voice server; PID 1
continues delivering only the selected pair to the restricted `nuschl-tls`
publisher. nginx has filesystem exclusions for the voice certificate source and
published runtime directory, and the server uses the runtime pair rather than
source-key access. Do not add these services to the certificate's private group.

The option selects a webroot and clears inherited DNS/standalone/S3 challenge
choices **for this certificate only**. Other certificates keep their settings.
An explicitly conflicting per-certificate definition must be resolved rather
than silently overridden. With the option off, ordinary `tls.enableACME` supports
other configured challenges without requiring nginx. No firewall is opened
implicitly, and IPv6 listening is added only when the host enables IPv6.

Do not merge HTTPS/ACME-reuse settings into the voice name's HTTP-only nginx
virtual host. The module rejects that combination. The selected-pair validator,
atomic publication and SIGHUP renewal behavior remain the same as below.

## Relay and DNS boundaries

The enabled relay listens on a group-protected Unix socket behind nginx, not an
extra public TCP listener. Default allowed browser origin is exactly the web
host's HTTPS origin; destination ports default to 64738–64799. Browsers still
verify the destination server's TLS themselves. Current browser voice uses
Mumble's TLS `UDPTunnel`; the relay's optional UDP facility is separate.

Both application checks and the systemd IP filter reject private, loopback,
link-local and other special destinations. Keep `relay.allowNetworks = []` for
this public setup. Do not disable those checks or set an unrestricted allowance
to work around local DNS or cohosting. A public destination name should resolve
to its actual reachable public address.

If `services.resolved.enable = true`, the module allows the dedicated DNS stub
address `127.0.0.53` through the unit's kernel filter. Otherwise the default DNS
exceptions are empty. If your existing system resolver uses another local/private
address, configure `services.nuschl-web.relay.resolverAddresses` for that exact
resolver and ensure the OS resolver already uses it. This option neither starts
nor selects a DNS server. Its kernel exception is address-wide, so prefer a
dedicated resolver address. It does **not** let application tunnels target that
address; the relay's own destination checks still apply. See [web.md](web.md).

## Optional witness: bootstrap and trust

A witness is not required to run the compatible voice/chat server. Add
`inputs.nuschl.nixosModules.witness` and deploy the witness first, without making
the server read a nonexistent verifier-key file:

```nix
services.nuschl-witness = {
  enable = true;
  virtualHost = "witness.example.org";
  enableACME = true;
};
```

The shared global ACME settings and public 80/443 rules above also cover it.
Default plain HTTP listener is **127.0.0.1:8473** behind nginx; do not expose that
listener directly. Its default cosigner name is
`witness.example.org/nuschl-kt`. The witness generates its persistent ML-DSA-44
seed once; changing either the seed **or cosigner name** changes the verifier key.

Read the public key from the witness's local journal and compare it with its
authenticated HTTPS `/nuschl-kt/v1/vkey` response. Save that exact single-line
verifier key as `nuschl-witness.vkey` in the configuration repository, then add:

```nix
{ lib, ... }: {
  services.nuschl.kt.witnesses = [{
    key = lib.removeSuffix "\n" (builtins.readFile ./nuschl-witness.vkey);
    url = "https://witness.example.org";
  }];
}
```

The server origin remains `voice.example.org:64738`; add each genuine connection
alias explicitly if clients use several. The witness URL is a base HTTPS URL,
and its verifier key contains its own cosigner name. These are different roles.
Keep public witness `enrolPrivateAddresses = false`, its default.

The server entry enables submissions; it does **not** force clients to require
that witness. Custom client configuration is supported through
`programs.nuschl.keyTransparency`, settings/managed JSON, or CLI `--kt-witness`;
see [witness configuration](witness.md#configuration). Built-in
`DEFAULT_WITNESSES` is currently empty. Configure the intended common witness
policy on clients explicitly; a cohosted witness shares the server's failure and
compromise domain and is not an independently operated auditor. Witnessing
authenticates directory consistency; it does not make compatible chat encrypted.

## Storage, backups and administration

Defaults are SQLite at `/var/lib/nuschl/server.sqlite3`, with persistent server
state in `/var/lib/nuschl`. Server KT signing/VRF keys are generated once there
unless supplied through runtime files. Back up the database **and** these keys;
they are distinct from the renewable TLS certificate. Witness state, its seed
`witness.key` and per-origin log/checkpoint files live under
`/var/lib/nuschl-witness` unless the seed is supplied with `keyFile`.

Use a consistent SQLite backup method or stop the server and wait for its workers
to exit before an offline copy. Do not copy only a live `.sqlite3` file while
ignoring WAL contents. Include state/key files and protect backups as secrets;
compatible chat is readable to the server and its backups. `DynamicUser`
StateDirectories may resolve into `/var/lib/private`: verify that the backup
contains the resolved data, not merely the symlink. Keep your encrypted secret
sources, configuration and public verifier keys with the recovery material.

For PostgreSQL, `database.type = "postgresql"` supports local provisioning with
`createLocally = true`, or an external runtime `urlFile`; see [server.md](server.md).
Back up PostgreSQL consistently as well as the filesystem KT keys. Do not infer
that choosing PostgreSQL backs up server or witness identity material.

Restoring old KT/witness state is not ordinary certificate renewal: published
append-only checkpoints and client pins can be newer than a backup. Do not reset
keys or roll a witness behind published state merely to silence consistency
errors. Preserve the newest recoverable directory/checkpoint state and assess
rollback before exposing a restored service.

Follow [Bootstrap a named administrator](server.md#bootstrap-a-named-administrator):
set the SuperUser secret, register the normal client identity, then connect as
SuperUser in Nuschl's desktop client. Open the root channel's ACL/group editor,
add the registered account to the root `admin` group and save. Disconnect
SuperUser and verify the normal account can perform the intended administrative
actions. Keep unrelated group flags/ACLs intact; never grant Write to `all`.
This native route also works on E2EE-only servers. Stock Mumble's group editor
is an alternative on compatible servers. Web/TUI editing parity remains open;
see [the native editor's transaction limits](acl-editor.md).
Offline admin commands must use the same state/database/service identity and
cannot run alongside the live server.

`security = "e2ee"` is a separate policy: stock Mumble is refused, registered
Nuschl devices use MLS text, and voice is disabled until end-to-end media exists.
Do not switch modes just to work around an administration/UI limitation.

## Deployment checks and renewal

Before switching the host, evaluate/build **your complete host configuration**,
check assertions and the chosen revision, and arrange backups. This guide's
fixture does not establish DNS, agenix ordering, ACME issuance or deployed health.

On your own deployed host, inspect the relevant units and logs:

```sh
systemctl status nuschl nuschl-tls-refresh nuschl-web-relay nuschl-witness nginx
systemctl list-units --all 'acme-*'
journalctl -u nuschl -u nuschl-tls-refresh -u nuschl-witness -u nuschl-web-relay
ss -lntup
```

Only query optional units you enabled. Check public DNS from the server as well
as a client. As an operator-controlled health check, verify HTTP redirect, HTTPS
`/app/` and witness public-key response; test native TLS with the correct SNI and
full-chain validation, then join from a disposable client without media capture.
Never treat a successful HTTPS page as proof that port 64738 serves the same chain.

The module loads `fullchain.pem`, not the leaf-only `cert.pem`. Its isolated
publisher checks chain, hostname, expiry and key match, atomically publishes the
selected pair, then sends SIGHUP. Existing sessions stay connected. Initial
issuance failure keeps the server waiting rather than serving an ACME bootstrap
self-signed certificate. A rejected renewal retains the previous valid pair; a
five-minute timer retries. Inspect failures before the active certificate expires.
Manual certificate credentials have different refresh behavior; see
[server TLS](server.md#reusing-an-acme-certificate).

The publisher validates against NixOS's configured CA bundle. Native clients use
their own compiled WebPKI roots; adding a private CA to the server host's trust
store does not automatically authorize that CA on clients.

An unpinned client uses WebPKI hostname/chain verification; explicit saved pins
remain strict across renewal. Do not disable verification or automatically clear
pins. Resolve an unexpected certificate prompt using the actually served chain,
hostname and saved trust policy.

## Reproduce this configuration review

```sh
python3 tests/server_operators.py --output /disk/cache/operator-review
```

This evaluates the actual pinned NixOS modules offline for the supplied settings,
an interface-scoped 80/443 completion, and systemd-resolved DNS exceptions. No
service, ACME order, DNS request, public Mumble connection or deployment runs.
Package stubs and an explicitly invalid fixture-only verifier string avoid builds
and key generation: this is module-wiring evidence, not runtime key validation.
The existing private [PKI fixtures](server.md#reusing-an-acme-certificate),
NixOS VM tests and [witness tests](witness.md) provide separate runtime evidence.

The HTTP-01 option's seven module-evaluation cases can be reproduced with:

```sh
python3 tests/server_http01.py --output /disk/cache/http01-module-review
```

They cover cohosted HTTPS services, IPv4-only hosts, unrelated DNS challenge
defaults, disabled nginx integration and rejected conflicting choices. The
separate `tests/server_http01_vm.nix` fixture uses two private NixOS guests and
real Pebble/lego HTTP-01 issuance, with validation enabled. Build its driver with
a previously built native server package (including its adjacent storage helper):

```sh
nix build --no-link --print-out-paths --cores 2 --max-jobs 2 --impure \
  --file tests/server_http01_vm.nix \
  --argstr packagePath /nix/store/REPLACE-with-built-nuschld-package driver
```

Run the returned driver's `bin/nixos-test-driver -o "$scratch/out"` with a fresh,
owned mode-0700 **disk-backed** `$scratch`, `TMPDIR="$scratch/tmp"`,
`XDG_RUNTIME_DIR="$scratch/run"`, and `NUSCHL_VM_SCRATCH_ROOT="$scratch"`.
Create `tmp`, `run` and `out` as mode 0700 too. The test asserts the actual driver's
state paths and filesystem **before booting**. Do not reuse an earlier run's
state: Pebble generates a new ephemeral issuer each time. Limit the driver to ten
minutes and monitor free scratch space; the completed run took about a minute.
Client `TMPDIR` does not relocate Nix daemon build scratch.

The completed 2026-11-12 run passed real issuance and renewal with three distinct
public keys/SANs, live challenge GET/catch-all/POST checks, active nginx
configuration/SNI checks, and actual service UID/group and mount-namespace key
access probes. nginx could read its web/witness keys but could not read the voice
source/runtime keys; the voice server could read its runtime key but not the
source key. Renewal performed fresh HTTP validation, changed the served leaf,
kept the server PID and preserved a logged-in client's ping.

The issuer is private Pebble, **not a public CA**. The fixture binds its ephemeral
public root over the CA bundle only inside the publisher's service namespace;
the production validation script and outside CA bundle bytes stay unchanged.
The cohosted web/witness fixtures are nginx HTTPS endpoints, not an execution test
of the full web/witness applications. This does not test public DNS, firewall
reachability, a public issuer, deployment or migration.

Earlier failures are retained separately: an initial chain-check failure lacked
stderr; the diagnostic rerun passed that unchanged check, then exposed a test
helper's HTTP-module shadowing. Another run correctly failed renewal because
Pebble's five-year leaf exceeded the fixture's one-year renewal threshold. The
final fixture uses a ten-year **test-only** threshold to force actual renewal;
production renewal policy is unchanged.
