# The Nuschl server (`nuschld`)

For a complete NixOS deployment walkthrough and a review of the combined
voice/web/witness configuration, start with the [operator guide](server-operators.md).
This page covers the server's configuration, administration and protocol details.

`nuschld` is a Mumble-compatible server: protocol 1.5, with the legacy UDP voice
format for 1.4 and 1.3 clients. Stock Mumble clients use it like a stock Murmur:
certificates and passwords, channels, ACLs and groups with Murmur's semantics, text,
whisper and shout, listeners, and voice over UDP and the TCP tunnel, with state in
SQLite or PostgreSQL behind the storage worker. It routes Opus voice without
decoding it. Nuschl clients also get the Nuschl extension (docs/extension.md,
phase 2 of the roadmap): durable conversations, server history, edits and
deletions, subscriptions, read markers and direct conversations. The `vm-server`
check runs Nuschl's protocol suites and the stock Mumble GUI against it (Tests,
below).

## Running it

```sh
nuschld --config nuschld.toml          # see apps/server/nuschld.example.toml
nuschld --config nuschld.toml --check-config
nuschld --config nuschld.toml admin users list     # operator commands, see Administration
nuschld --config nuschld.toml admin import-murmur /var/lib/mumble-server/mumble-server.sqlite
```

Every configuration key is optional; names and defaults follow murmur.ini
(`users` is `max_users`, `bandwidth` is `max_bandwidth`, `messagelimit` is
`message_limit`, and so on). Command-line flags override the file:
`--listen-address`, `--port`, `--state-directory`, `--sqlite-path`,
`--database-url-file`, `--tls-certificate-file`/`--tls-key-file`, `--password-file`,
`--superuser-password-file`, and `--ephemeral` (state in memory only).

- **Security mode** (a server-level switch, docs/e2ee.md §10, answer 7).
  `security = "compatible"` (the default for now) admits stock clients and has no
  end-to-end encryption at all; Nuschl clients get the extension's plaintext extras
  (server history, edits, typing, durable direct messages) and label them "not
  end-to-end encrypted". `security = "e2ee"` needs key transparency (`[kt]`, on by
  default) and:
  - turns stock clients away at the connection stage (a `Reject` they can show,
    their `Authenticate` never read), and Nuschl clients that do not agree on
    `e2ee/1` at `Authenticate` (`WrongVersion`);
  - runs the MLS delivery service (`mls/1`): every conversation is an MLS group;
  - refuses plaintext: `ChatSend`, `ChatEdit` and `ChatDelete` (`unsupported`) and
    stock `TextMessage` (`PermissionDenied`); `typing/1` is not offered;
  - admits unregistered connections only to register: they read, write, speak and
    hear nothing (ACLs apply to everyone as usual).

  Voice, whispers and shouts are blocked in this mode until SFrame with MLS keys
  (phase 4e); clients also disable their audio worker. Read-marker sequence numbers
  remain visible to the server until self blobs; files are not offered. Switching a server between the modes keeps its
  state; clients remember `e2ee/1` (sticky), so a server that drops back to
  compatible mode is refused by clients that knew it as end-to-end.
- **Secrets** come from files: `password_file`, `superuser_password_file`,
  `database_url_file`, `private_key`, `kt.signing_key_file`, `kt.vrf_secret_file`.
  The NixOS module passes them through systemd `LoadCredential`
  (`services.nuschl.kt.signingKeyFile`, `vrfSecretFile`, …) and refuses Nix store
  paths.
- **Certificate.** Without `certificate`/`private_key`, a self-signed ECDSA
  certificate is generated once in `state_directory` (key file 0600). Clients
  explicitly pin a self-signed certificate, so keep that directory. An unpinned
  client instead requires a valid CA chain for its configured hostname; normal
  CA leaf renewal neither creates a pin nor needs another confirmation. SIGHUP loads the certificate and
  key again (a renewed ACME certificate, say): new handshakes use them, connected
  clients keep their session, and if they do not load (unreadable, malformed, a key
  that does not belong to the certificate) the current one stays and the error is
  logged. A generated certificate is re-read, never generated again. Under the
  NixOS module, manual `tls.certificateFile` and `keyFile` are credentials copied at
  start, so a renewal there needs a restart.
- **TLS versions.** 1.2 and 1.3 by default; `minimum_tls_version = "1.3"` refuses
  TLS 1.2, which keeps client certificates (stable identities) off the wire in the
  clear. Stock clients built against OpenSSL 1.1.1 or later (Mumble 1.3.1 and later
  on current systems) speak 1.3; older builds cannot connect then.
- **Strong certificates.** `strong_certificate_authorities` (empty by default, so
  nobody is strong, as in Murmur without a CA store) lists the authorities whose
  client certificates count as strong, Murmur's `bVerified`: `"webpki"`
  (Mozilla's roots, built in), `"system"` (`SSL_CERT_FILE`, or the system's
  bundle such as /etc/ssl/certs/ca-certificates.crt) or PEM files of trusted
  authorities (murmur.ini's `sslCA`). A certificate is strong when the chain the
  client presents leads from it to one of them and nothing in it is expired or
  not yet valid, whatever the certificate's purpose (Murmur accepts e-mail
  certificates too). That is decided once, at login, after the handshake proved
  the client holds the key. A strong user matches the ACL group `strong`,
  `UserStats.strong_certificate` says so, and the login's log line has `strong`.
  Self-signed certificates log in as before (the identity is still the
  certificate's hash), and `certificate_required` still asks for any
  certificate; to require strong ones, deny Enter to `!strong` at the root.
  Murmur also logs a strong certificate's name, e-mail addresses and issuer, and
  logs a strong certificate with an e-mail address in to the account registered
  under that address when its hash is unknown; neither happens here: logs carry
  no names or addresses, and that login would hand an account to anyone an
  authority issued a certificate for the same address (review P1-13; e-mail
  addresses are not imported from Murmur either). An account that is offline
  (`since_access` bookkeeping) is never strong; strength belongs to a
  connection. The authorities are read at start.
- **SuperUser** (user 0) has no password in a fresh database until one is set;
  without one it cannot log in. The stored password persists: removing a startup
  password-file setting does not disable a password set earlier. See
  [Bootstrap a named administrator](#bootstrap-a-named-administrator).

- **Key transparency** (`[kt]`, docs/e2ee.md §5): on by default; the server runs a
  verifiable key directory and offers `kt/1`. Its head-signing key (the server
  identity key, which clients pin) and VRF secret come from `kt.signing_key_file` and
  `kt.vrf_secret_file` (32 bytes or 64 hex digits; never Nix store paths;
  `--kt-signing-key-file`, `--kt-vrf-secret-file`), or are generated once in
  `state_directory` (`kt-signing.key`, `kt-vrf.key`, 0600). Keep them: a log can
  never change its VRF secret, and clients treat a changed key as a replaced log.
  `publish_interval` (10 s) and `refresh_interval` (an hour) pace epochs and re-signed
  heads; `history = "contacts"` (default) or `"members"` says who may fetch an
  account's history besides its owner.
  Every registered user's registration number and lower-case name are published as
  `reg:` and `name:` labels (renames release the old name). **Witnessing**
  (docs/witness.md): `origins` lists the `host:port` names clients use (NixOS: the
  host's FQDN and port); the server submits its epoch log to the built-in witnesses
  (`default_witnesses`, none until the project auditor is deployed) and to
  `[[kt.witnesses]]` (`key`, `url`), every `witness_interval` (60 s) while epochs are
  new and every `witness_refresh_interval` (an hour) otherwise, over HTTPS with the
  webpki roots, and attaches their cosignatures to the heads it serves
  (`kt-witness/1`). The journal says when a witness starts or stops cosigning.
- **Statistics.** Every `statistics_interval` seconds (300; 0 turns it off) the log
  gets one `statistics` line with what happened in that interval: users online,
  channels, connections, logins, rejected logins, control messages and bytes, voice
  packets and bytes in and out, rate-limited messages and whisper packets dropped
  for the fan-out budget. Counts only, no names or addresses
  (`journalctl -u nuschl | grep statistics`).
- **End-to-end encryption** (`[mls]`, docs/e2ee.md "Phase 4
  prototype"): in end-to-end mode the server offers `mls/1` and runs the MLS delivery
  service (`claims_per_minute` 30, `uploads_per_minute` 10, `welcome_days` 90). There
  is no member cap (the user's decision, docs/e2ee.md §6.7): message size limits bound
  a group.
- **History** (`[history]`): kept forever by default (`retention = "forever"`; or
  `"none"`, not archived, or a number of days; channels may set their own, see
  Retention below, never more than `max_retention_days`); guests read
  none (`guests = "none"`); stock clients are told in the welcome text and when they
  enter a channel with history (`notify_stock_clients`); stock senders' direct
  messages are not stored (`archive_stock_direct_messages`); `edit_limit`,
  `moderators_delete`, page limits per connection and per network
  (`network_pages_per_minute`, `network_page_bytes_per_minute`,
  `unlimited_networks`) and the client versions below.
- **Client versions** (`[history]`, docs/extension.md "Minimum versions"), as Signal
  keeps its clients current. `recommended_client = "0.2.0"`: older Nuschl clients
  suggest an update (users can dismiss it). `minimum_client = "0.2.0"`: older Nuschl
  clients are refused at `Authenticate` with `Reject` `WrongVersion` ("This server
  requires Nuschl 0.2.0 or newer. Update Nuschl to connect."). With
  `minimum_client_from = 2026-12-01` (a date, midnight UTC; quoted or not) they are
  admitted until that date and see it in a warning they cannot dismiss; without it
  the minimum applies at once. A client that reports no version counts as older than
  any minimum. Versions are self-reported (the hello's `client_version`) and compared
  by semver; this keeps the ecosystem current and is not a security control. Stock
  Mumble clients are never refused for their version (the security mode decides
  about them). `recommended_client` below `minimum_client`, or a date without a
  minimum, is a configuration error.

## The NixOS module

The flake exports `nixosModules.default` and a headless `packages.<system>.server`
containing `nuschld` and the storage worker, without the GPUI and audio stack. The
module defaults to SQLite and also supports PostgreSQL, either through
`database.urlFile` (a runtime credential file with the URL) or local peer
authentication:

```nix
imports = [ inputs.nuschl.nixosModules.default ];
services.nuschl = {
  enable = true;
  security = "compatible"; # stock Mumble clients, no end-to-end encryption (yet)
  openFirewall = true; # off by default
  database.type = "postgresql";
  database.createLocally = true;
};
```

Other options include `listenAddresses`, `port`, `settings` (the TOML keys above),
`passwordFile`, `superUserPasswordFile`, `tls.certificateFile` and `keyFile`,
`kt.signingKeyFile` and `vrfSecretFile`, and `importMurmur.databaseFile`; all are
described in [nix/module.nix](../nix/module.nix). Secrets are runtime files passed
with `LoadCredential`, never Nix store values.

The module (`services.nuschl`, nix/module.nix) generates the TOML file
from `settings` (never secrets), runs a database preflight under the operator lock
(so the service does not start while `nuschld admin` uses the database), and
hardens the unit
(DynamicUser, a private state directory, no capabilities, system call filter,
`SocketBindAllow` for the server port only).

### Reusing an ACME certificate

For a hostname already served by nginx (or `services.nuschl-web`) with
`enableACME = true`, select the existing certificate entry explicitly:

```nix
services.nuschl = {
  enable = true;
  hostName = "voice.example.org";
  tls.useACMEHost = "voice.example.org";
};
```

This selects `security.acme.certs."voice.example.org"` and its `fullchain.pem`
and `key.pem`. It does not change that entry's group or add the server to the
broad `acme`/nginx group. A differently named entry or SAN certificate can be
reused with `tls.useACMEHost = "shared.example.org"` and an explicit covered
`hostName`; wildcard entries also require a concrete connection hostname.
Clients must connect using that hostname, not an IP address or uncovered alias.

To have the server module select a certificate for its own configured hostname:

```nix
services.nuschl = {
  enable = true;
  hostName = "voice.example.org";
  tls.enableACME = true;
};
security.acme = {
  acceptTerms = true;
  defaults.email = "operator@example.org";
  # Configure the normal NixOS HTTP/DNS challenge, e.g. defaults.dnsProvider,
  # with runtime credentials, or an existing nginx ACME webroot.
};
```

`tls.enableACME` requires `hostName` and an ordinary working NixOS ACME challenge
configuration. Choose it or `tls.useACMEHost`; neither combines with manual
`certificateFile`/`keyFile`. No certificate key or credential value belongs in a
Nix string or the Nix store.

The module waits for certificate provisioning. A dedicated, unprivileged
`nuschl-tls` service receives only the selected full chain and key through fresh
systemd credentials. Its networkless, resource-limited helper validates the
chain, hostname, expiry and key pair with OpenSSL, then atomically publishes one
private runtime generation under `/run/nuschl-tls`. The helper owns that directory;
`nuschld` has read-only group access to this selected pair, not other ACME keys.
The fixed privileged post-step sends SIGHUP only to an already running server.
Normal renewal never restarts it or disconnects existing clients. Invalid or
incomplete updates retain the last published pair; `nuschld` also keeps its active
in-memory certificate if reloading fails. A five-minute timer retries a missed
renewal hook or a temporarily incomplete source pair. The helper refuses to
publish an untrusted ACME bootstrap/self-signed certificate, rather than exposing
an unexpected trust prompt while issuance is incomplete.

If initial issuance or publication fails, server startup retries every 30 seconds
without running database checks until a validated runtime pair is readable.
There is no startup retry limit for this ACME wait. A previously published pair
can still start the server while a new source pair is rejected. The renewal hook
and timer never start an intentionally stopped server. The timer currently
republishes and reloads a valid pair even when its bytes have not changed.

The helper's trust bundle is NixOS `security.pki.caBundle`. Clients still enforce
their own WebPKI roots; a custom/private server CA is not automatically imported
into clients. The helper parser handles private keys only in its separate service,
with no network/home access, a 30-second start timeout, 128 MiB memory limit and
bounded credential sizes. The server service's broad sandbox remains in force.
Manual `tls.certificateFile`/`keyFile` retain their original startup-credential
behavior; a source-file change requires a service restart to refresh those copies.

Existing client pins are deliberately preserved, including pins saved by older
versions that automatically pinned a valid CA leaf. Such a bookmark remains
strict and will ask when that exact certificate changes. To choose normal CA
renewal, explicitly clear that bookmark's fingerprint and reconnect using its
correct DNS name. This is a trust-policy choice, never an automatic pin removal.
Unpinned local history uses the configured WebPKI origin, so routine CA renewal
keeps its history; pinned history retains its original certificate namespace.
Changing from an old pin to CA trust does not delete the old namespace's rows or
silently merge them into another authority's history.

Private validation is in `tests/server_pki.py` / `tests/server_pki.nix` (module
configuration and exact runtime publisher with a generated local CA), and the
native/browser TLS tests (full chain, renewal, wrong hostname, expiry, untrusted
issuer, missing intermediate and strict pins). These fixtures do not contact a
public ACME service or change host trust/configuration. The module fixture alone
does not execute systemd lifecycle/UID mounts; those require a NixOS VM.

The standalone `tests/server_pki_vm.nix` driver covers those installed-unit
boundaries: DynamicUser/group access, the read-only runtime mount, scoped PID1
credentials, invalid bootstrap followed by automatic startup recovery, renewal
through the real ACME post-run hook and privileged HUP, retained connections,
invalid-update retention, and intentional stop. Only the issuer command is
replaced with an offline marker; the timer is accelerated to five seconds and a
one-second helper pre-step permits inspection of its credential mount. Actual
public issuance and challenge-provider behavior remain outside this test.

Use an already built package containing `nuschld` and `nuschl-storage-worker`.
Only the public fixture CA enters the store; keep its private key and VM runtime
outside the checkout. Build the driver, not the VM test derivation, so the running
VM and its sparse disk can use disk-backed scratch:

```sh
cachedPackage=/nix/store/your-already-built-nuschl-package
pkiWork=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/nuschl-pki-vm.XXXXXXXX")
mkdir -p "$pkiWork/tmp" "$pkiWork/work" "$pkiWork/output"
python3 tests/server_pki_vm.py prepare --directory "$pkiWork/fixture"
driver=$(nix build --impure --file tests/server_pki_vm.nix driver \
  --argstr packagePath "$cachedPackage" \
  --argstr trustedCaFile "$pkiWork/fixture/root.pem" \
  --cores 2 --max-jobs 2 --no-link --print-out-paths)
cd "$pkiWork/work"
TMPDIR="$pkiWork/tmp" NUSCHL_PKI_FIXTURE_DIRECTORY="$pkiWork/fixture" \
  "$driver/bin/nixos-test-driver" --output_directory "$pkiWork/output"
```

Client `TMPDIR` does not relocate Nix daemon build scratch. Inspect the driver
dry run and available daemon scratch before building; this test does not require
rebuilding the server package, kernel, QEMU, or systemd when those are cached.

## Importing a Murmur server

`nuschld admin import-murmur PATH` (or the flag `--import-murmur PATH`) moves a
Murmur (mumble-server) server into this one: registered users with their passwords, certificate hashes, last channels,
comments and avatars; the channel tree with descriptions, positions, user limits and
ACL inheritance; ACL entries, groups and members (inherit and inheritable flags);
links; channel listeners; and bans. Stop Murmur first (the file is read as it is on
disk) and run the import with the configuration the server will use, as the user it
runs as. Like every admin command it takes the operator lock before it opens the
database, so it refuses to run while a server (or another admin command) uses it.
It prints a report and exits.

- **Which Murmur.** Both database layouts: `ServerDB.cpp` (Murmur 1.2.4 to 1.5,
  schema versions 6 to 9, `murmur.sqlite`/`mumble-server.sqlite`) and Murmur 1.6's
  (schema 10 and later, where ACLs refer to groups by id). Older files: start them
  once with Murmur 1.5 to upgrade. SQLite only; for a Murmur on MySQL or PostgreSQL,
  export to SQLite first. A database holding several virtual servers is imported one
  at a time: `--murmur-server-id N` (default 1).
- **Into what.** The import writes through the storage worker like the running
  server, so the target may be SQLite or PostgreSQL. It refuses a server that already
  has channels, registered users or bans (besides the root channel and SuperUser)
  and changes nothing; a Murmur database without a root channel for the chosen
  virtual server (Murmur creates it on that server's first start) is refused the same
  way. `--replace-existing` deletes those first: deleted channels'
  history becomes an archive only SuperUser reads, deleted accounts' messages show as
  a deleted user's, as when they are removed by hand. Ids are kept (ACLs, groups and
  `default_channel` refer to them); registration ids continue above the highest
  imported one. If any write fails (the log says which), the import exits with an
  error, so it can be run again with `--replace-existing` once the cause is fixed.
- **Untrusted input.** The storage worker reads the file (`--read-murmur`)
  confined by Landlock to reading that one file (no other file, no TCP, no abstract
  Unix sockets; the report says whether the kernel enforced it). It opens it
  read-only and immutable (a `-wal` or journal next to it is never looked at), with
  `query_only`, `trusted_schema = OFF`, cell size checks, a 1 GiB heap limit and
  `quick_check` first. Files in WAL mode (Murmur's `sqlite_wal`) are refused with the
  command that converts them, so SQLite's WAL code never runs on the file; only
  ordinary tables without hidden or generated columns are read (by the parsed schema,
  not its text). Fixed statements read 32 rows at a time, every column capped in SQL
  (names 4 KiB, hashes and salts 256 bytes, numbers and dates 64 bytes, comments,
  descriptions, avatars and settings 1 MiB), and every value is decoded by its actual
  type, so a malformed row is reported, not fatal; values over their cap, and blobs
  beyond 256 MiB in total, are dropped and reported. `nuschld` then applies its
  own rules: `username_pattern` and `channel_name_pattern`, unique names (users
  regardless of case, siblings exactly), the text limits for comments and
  descriptions, `image_message_length` for avatars (Mumble's pre-1.2.3 raw avatars are
  dropped), at most 512 ACL entries and groups per channel and 8192 members per group,
  one root and no cycles (a channel that fails takes its subchannels with it),
  references only to imported users and channels. The report names what was skipped
  by id, never by content.
- **Passwords** keep Murmur's hash until the user's next login, which replaces it
  with argon2id: PBKDF2-HMAC-SHA384 with Murmur's hex salt and 48-byte output
  (`kdfiterations` up to 2,000,000; more would make one row cost seconds per login
  attempt, and such an account is imported without a password), or the legacy
  unsalted SHA-1 that Murmur keeps for accounts whose `kdfiterations` is not set.
  Both are compared in constant time. The report counts the SHA-1 accounts; until
  their users log in, the database holds their unsalted hashes. SuperUser keeps this
  server's password if it has one; `superuser_password_file` replaces it at every
  start anyway.
- **Settings.** Murmur keeps per-server settings in its `config` table (set through
  Ice or written by Murmur itself). Those with an equivalent are written to
  `murmur-import.toml` in `state_directory` for you to merge (`welcometext`,
  `users`, `bandwidth`, `textmessagelength`, the patterns, the limits, …); the server
  password goes to `murmur-server-password` (0600) for `password_file`. Murmur's
  certificate and key in the database (Murmur stores the self-signed one it
  generated) become this server's generated certificate if there is none yet, so
  clients that pinned Murmur's keep trusting the server; otherwise they are written
  next to it for `certificate`/`private_key`. murmur.ini is not read: its values
  carry over by hand, under the same names in snake_case.
- **Not imported:** e-mail addresses (no e-mail login here), disabled listeners
  (Murmur keeps their volume), Murmur's log table, and settings without an
  equivalent (listed in the report).

On NixOS, `services.nuschl.importMurmur.databaseFile` runs the import once, in its
own unit (`nuschl-import-murmur.service`, without a start timeout) before the
server: the file is copied into the state directory for the dynamic user, imported,
and removed; `murmur-imported` in the state directory records that it ran. If the
import fails or refuses (the server already has state; `importMurmur.replaceExisting`
replaces it), `nuschl.service` does not start and the journal says why; nothing
retries in a loop.

## Administration

`nuschld admin` works offline on the database, through the storage worker as
the server does, and changes it with the server's own state code (validation,
cascades, persistence). Options before `admin` (`--config`, `--state-directory`,
`--sqlite-path`, `--database-url-file`) select the database as for the server.

```sh
nuschld --config nuschld.toml admin check-config     # also: does the database open, is it in use
nuschld --config nuschld.toml admin users list
nuschld --config nuschld.toml admin users show Alice  # or '#3'
nuschld --config nuschld.toml admin users rename Alice Alicia
nuschld --config nuschld.toml admin users remove Alice --yes
nuschld --config nuschld.toml admin users set-password '#0' --password-file /run/secrets/nuschl-superuser
nuschld --config nuschld.toml admin channels list
nuschld --config nuschld.toml admin channels retention Lobby 30 --yes   # forever, none, default, days
nuschld --config nuschld.toml admin bans list
nuschld --config nuschld.toml admin bans add --address 192.0.2.0/24 --reason spam --duration 3600
nuschld --config nuschld.toml admin bans remove 0
nuschld --config nuschld.toml admin history export Lobby --output lobby.json
nuschld --config nuschld.toml admin history erase-user Alice --yes   # GDPR requests
nuschld --config nuschld.toml admin import-murmur murmur.sqlite   # see Importing a Murmur server
```

- **Subcommands, not a second binary**: one executable to deploy and keep at
  the server's version (the schema and its migrations match), the same
  configuration and storage-worker discovery, and nothing new on the network
  (the review's P0 asks for no network RPC for administration; this has none).
  The older flags `--set-superuser-password` and `--erase-user` remain.
- **Never against a live server.** The server and an admin command both keep
  the state in memory and write it back, so they must not share a database. The
  server's state worker, and each admin command's, takes an operator lock before
  it opens (or migrates) the database: an exclusive `flock` on
  `<database>.lock` next to a SQLite file (SQLite's own locks are per
  statement, not per process lifetime), a session advisory lock on PostgreSQL.
  Whoever comes second gets "another process … is using this database" and
  changes nothing; `check-config` only reports it. A second server on the same
  database is refused the same way, after retrying for ten seconds (a previous
  instance's workers may still be exiting when a script restarts it). If the state worker crashes, the server
  restarts it, which takes the lock again (or, if an admin command took it in
  between, every write fails and is logged rather than interleaved). A lock file
  that cannot be opened or locked at all (an unusual file system) is warned
  about and ignored rather than keeping a server from starting.
- **Destructive commands need `--yes`** and say first what they do: removing a
  user (ACL entries and group memberships go; the account never comes back; its
  messages stay), erasing a user's messages, and shortening a channel's retention
  (with the number of stored messages that will expire).
- **Exports** are JSON: the conversation, its retention and head, and every row
  (author account, number and name, format, content, times, edits, deletions),
  without idempotency keys and without content past the retention. A file given
  with `--output` is created 0600. A conversation id exports a deleted channel's
  archive or a direct conversation.
- Run admin commands as the user the server runs as (on NixOS the unit uses
  `DynamicUser`): files created as another user (SQLite's `-wal` and `-shm`,
  say) can keep the server from opening the database.

### Bootstrap a named administrator

Nuschl's native ACL/group editor supports this in both `compatible` and `e2ee`
mode. Stock Mumble's group editor is an alternative for a server already running
in `security = "compatible"` (the default). Stock Mumble cannot connect to an
E2EE-only server; keep that security policy and use the native Nuschl client.
Web/TUI group editors and offline CLI group administration remain queued.

1. **Set a SuperUser password.** On NixOS, provision a root-owned, mode `0600`
   runtime file, preferably through your secret manager. For a manual initial
   setup, these commands open an editor without putting the password in a shell
   command or Nix expression:

   ```sh
   sudo install -d -m 0700 /run/secrets
   sudo install -m 0600 /dev/null /run/secrets/nuschl-superuser
   sudoedit /run/secrets/nuschl-superuser
   ```

   Put the password on one line. `/run` is cleared on boot, so arrange for your
   secret manager to restore this file before the service starts. Point the
   module at its runtime path:

   ```nix
   services.nuschl = {
     enable = true;
     superUserPasswordFile = "/run/secrets/nuschl-superuser";
   };
   ```

   Apply the NixOS configuration. The module passes the secret with systemd
   `LoadCredential`; it does not copy it into the Nix store. On later password
   changes, update the file and restart `nuschl.service`. The file overrides the
   stored SuperUser password **on every startup**. `passwordFile` is a separate
   server-access password, not the SuperUser credential.

   For a standalone deployment without that startup override, stop the server
   and run the following as its service user, with its actual configuration and
   database paths, then start it again:

   ```sh
   nuschld --config nuschld.toml admin users set-password '#0' \
     --password-file /path/to/private-password-file
   ```

   This stores the password in the database. A configured startup password file
   will replace it again on the next start. Offline commands must not share a
   database with a running server; see the locking rules above.

2. **Register the ordinary account.** Connect with the Nuschl client and identity
   you intend to use normally. Open your user menu and choose **Register this
   name** (or **Register your name**). Fresh servers permit self-registration;
   if your ACLs disable it, use SuperUser's registration action instead. Group
   membership refers to the registered account, not its temporary session ID.
   Keep this client's identity/certificate: a new Mumble installation normally
   has a different certificate and does not become the same account just by
   using the same name.

3. **Connect Nuschl as SuperUser.** Open **Server settings**, set **Username** to
   `SuperUser`, enter the step-1 secret in **Password (optional)** (announced as
   **Server password**), and choose **Save and connect**. The password is kept
   only for this connection. Use a separate client profile if you want to retain
   the ordinary account's saved username unchanged.

4. **Add the registered account to the root `admin` group.** Right-click the
   root channel, choose **Edit permissions…**, then select `@admin` in **Groups**
   (the rules list also has an `@admin` entry). Enter the ordinary account name
   in **Registered group member**, choose **Resolve member**, check the resolved
   account, then **Add member** and **Save permissions**. Wait for **Permissions
   saved and confirmed by the server**. Preserve existing members, group flags,
   and ACL rules. The default root ACL grants the `admin` group **Write**, which
   confers administrative permissions; do not grant Write to `all`/`@all`.

   On compatible servers, stock Mumble can perform the same operation: connect
   as `SuperUser`, enter its password, right-click the root channel, choose
   **Edit…**, open **Groups**, and select `admin`. Under **Members**, enter the
   account in **Select member to add**, click **Add**, then **OK**.

5. **Disconnect SuperUser and use the ordinary account.** Reconnect that original
   Nuschl identity if necessary. Its root administration permissions now come
   from the registered account's group membership and survive server restarts.
   Keep the SuperUser credential private for recovery.

The loopback regression [admin_bootstrap.py](../tests/admin_bootstrap.py) exercises
real `nuschld` startup and offline password setup, stock-protocol SuperUser login,
registration, promotion through the root group, exact preservation of the fresh
root ACL entries and group flags, a benign channel-creation permission check, an
unpromoted control, and persistence after restart. It also checks password
rotation and rejection of stock clients in `e2ee` mode. The Mumble UI labels above
were checked against its source; the automated proof uses protocol messages,
not an interactive stock Mumble GUI session.

[acl_e2ee_bootstrap.py](../tests/acl_e2ee_bootstrap.py) additionally exercises the
actual native SuperUser password form and ACL/group editor against an owned
nuschld that starts and remains in `security = "e2ee"`. It checks fresh offline
account resolution, exact baseline rule/flag preservation, an unpromoted control,
a benign channel creation by the promoted account and persistence after restart.
It does not claim MLS chat/video coverage. See [the editor's draft and legacy
wire limits](acl-editor.md) for conflict/reload behavior.

## Design

```
TCP accept ──► connection task ──┐            ┌──► writer task ──► TLS
 (admission, TLS, framing,       │  Mutex     │    (byte-bounded queue)
  per-type limits)               ├─► State ───┤
UDP socket ──► udp task ─────────┘  (handlers,└──► UDP sockets
 (decrypt, bind, info pings)         routing,      (after the lock)
                                     authz_epoch)
                                       │
                                       ├──► storage task ──────► storage worker (state)
                                       ├──► history writer ────► storage worker (history,
                                       │    (ordered; acks and     ordered writes)
                                       │     fan-out under the lock)
                                       └──► history reader ────► storage worker (pages)
                                            (bounded queue; pages re-checked
                                             under the lock before sending)
```

- **Authorisation** (authz.rs). Every permission decision goes through one
  function, `State::authorize(actor, action)`: the actor is a session, the
  `Action` names what it wants and its target (about 50 of them: entering,
  moving, speaking, whispering, texting, listening, muting, comments, kicks,
  bans, registration, channels, links, ACLs; the extension's reading, sending,
  editing, deleting as author or moderator, channel settings, read markers,
  typing, direct conversations; `kt/1` publications, lookups and histories). The
  answer is `Ok` or a `Denial` in the protocol's terms: Murmur's
  `PermissionDenied` (the same permission, channel and session, or the same
  type), a request Murmur ignores, or the extension's uniform `not_found`.
  Handlers keep only limits and validation (sizes, counts, rate limits, names,
  user limits), and fan-out keeps membership (who is in a channel or listens to
  it), which Mumble delivers to without asking the ACLs. The ACL engine
  (`authz/acl.rs`, Murmur's `ACL.cpp` and `Group.cpp`) can only be evaluated
  from authz.rs: its evaluation functions, the permission cache and the lookups
  on it are private there, and a test fails if SuperUser checks or inline
  permission tests appear anywhere else. The same module reports permissions
  to clients (`PermissionQuery`, `ServerSync`), keeps `suppress` in step with
  Speak after every change, and says what part of a `since_access`
  conversation a reader sees. It also ends listeners that lost Listen (below).
  Moving the checks there turned up three places
  where one decision was made two ways; they now agree:
  - Temporary access tokens (`UserState.temporary_access_tokens`) were removed
    after their message without the re-evaluation every other credential
    change gets, so a token that granted Speak left the user unsuppressed for
    good and the client's permission cache kept the token's grants. Now they
    are re-evaluated when the tokens are added and when they are gone, as
    Murmur's `TemporaryAccessTokenHelper` does, and the tokens apply only after
    the SuperUser, name and rate checks, in Murmur's order.
  - A stock client's direct message was archived in the accounts' direct
    conversation (with `archive_stock_direct_messages`) after checking only the
    connection it named, and then delivered to all of the account's
    connections; a Nuschl client's send to the same conversation needs
    TextMessage in the channel of each of them. The stock path now archives
    only when the Nuschl send would be allowed (`Action::SendDirect`);
    otherwise the named connection gets the message live, as from Murmur.
  - Key-transparency histories were authorised once, when requested, while
    history pages are checked again when they are sent. A binding history is
    now authorised again when the key-transparency task answers, so someone
    who stopped being a contact in between gets `not_found`.
- **One state, one lock.** Channels, users, registrations, bans and permission
  caches live in `State` behind a mutex. Handlers run synchronously under it and
  never across an `.await`; Murmur serialises the same way (its main thread plus
  a voice read-lock). Authentication's slow parts (argon2id, loading a registered
  user's blobs) run outside the lock and are re-checked under it.
  The security review asked for one task that owns authorisation state (D3,
  §3 item 1); the mutex is that owner, so it stays rather than becoming an
  actor. What the review needs from an owner holds: every change to
  authorisation state is applied under the lock and bumps `authz_epoch`;
  subscriptions are re-evaluated before the lock is released, so an
  `Unsubscribed` is queued before any later event; live fan-out of stored
  messages runs under the lock against the readers of that moment; and each
  step that leaves the lock is checked again when its result comes back:
  history pages (the epoch moved: `can_read` and the visible ranges again),
  key-transparency histories (authorised again, the gap found above),
  logins (the account and its password hash re-read before the session
  exists) and voice target caches (keyed by the routing generation). An actor
  would serialise the same commands in the same order with a message hop
  more, and would not close any of these windows by itself; the audit found
  no check whose effect lands after another lock holder could have changed
  the answer, other than the one fixed. What remains open is throughput:
  the UDP path takes the lock per packet, which an immutable routing snapshot
  (review §3 item 2) would avoid; that is a performance question, not a
  correctness one.
- **Output.** Each connection has a queue bounded in bytes (`max_queued_bytes`).
  Control frames are never dropped: a client that lets the limit fill is
  disconnected. Voice frames are dropped once 64 KiB are waiting. Broadcasts
  encode once and share the bytes.
- **Voice.** The UDP task decrypts (OCB2-AES128, `crates/protocol/src/crypt.rs`),
  parses without copying (`relay.rs`), routes, and re-encodes per receiver
  (legacy or protobuf, sender, context, volume, positional data) into reused
  buffers; encrypted datagrams are sent after the lock is released. Receivers
  follow Murmur's `processMsg`: channel, listeners (with volume), linked channels
  where the speaker may speak, whisper and shout targets (links, children,
  groups) cached per routing generation, loopback (target 31). Mute, suppress
  and self-mute stop routing; Speak in the user's channel becomes `suppress`.
  The per-user bandwidth limit (Murmur's 360-packet window) applies to UDP and
  the tunnel alike.
- **UDP binding.** A datagram from an unknown address is trial-decrypted only
  against sessions whose TLS connection came from the same IP (at most eight,
  with a per-prefix budget); success binds or rebinds the address. As in Murmur,
  voice goes over UDP until the client tunnels something over TCP, and again
  after it sends voice over UDP. After five seconds without a good packet the
  server asks for the client's nonce (at most every five seconds); a flood of
  undecryptable packets pauses decryption for that address.
- **Persistence.** `nuschld` links no database code (deny.toml keeps SQLx in
  `nuschl-storage` and that crate in the storage worker). It starts
  `nuschl-storage-worker --server` and writes every change through an ordered
  queue (`crates/ipc/src/server.rs` defines the messages); the worker owns the
  database and its credential. Startup loads channels, ACLs, links, bans and
  registered users (in pages); comments, avatars and channel listeners are loaded
  when a registered user connects. Graceful shutdown (SIGTERM) disconnects everyone and flushes the
  queue. The schema (`crates/storage/server-migrations`) works on SQLite and
  PostgreSQL and is tracked in its own migrations table, so it can share a
  database with the history schema. Several storage workers share the database
  (state, history writes and reads, key transparency, MLS); on SQLite every write
  transaction begins with `BEGIN IMMEDIATE`, so one that reads before it writes waits
  for the lock instead of failing when another worker committed in between.
- **Channel listeners** of registered users are kept, as in Murmur: adding one,
  changing its volume and removing it are stored (`channel_listeners`, removed with
  the user or the channel), and after `ServerSync` on the next login they are added
  again; everyone learns the channels, only the user the volumes (unless
  `broadcast_listener_volume_adjustments`). Unlike Murmur, Listen and the listener
  limits are checked again then; a listener that fails stays stored for when it is
  allowed. Listeners in temporary channels and those of unregistered users end with
  the connection; a user who registers keeps the current ones.
- **Listeners follow Listen** (the user's decision on review D3.4, 2026-11-11;
  Murmur keeps a listener whatever happens to Listen). After every change to
  authorisation state (an ACL or group edit, tokens, a temporary token gone,
  registration or unregistration, a move, a reparented or removed channel:
  whenever `authz_epoch` moved), before the lock is released, each listener whose
  user no longer has Listen on its channel ends: it hears no more voice or text
  from the next packet on, and the channel is no longer readable through it, so
  subscriptions that needed it end and pages in flight fail when they come back
  (`can_read` again). Everyone learns it as from Murmur, a `UserState` with
  `listening_channel_remove` and no actor, so stock clients' channel trees stay
  truthful; the user also gets a `PermissionDenied` for Listen in that channel.
  Stock clients log it ("You were denied Listen privileges in …"); its reason
  ("Listening to … stopped: the permission to listen there was removed.") is
  what Nuschl shows, and Nuschl clients can tell it from a refused request by
  its arriving unasked, right after the removal. No extension message was
  added: the stock messages say it to both kinds of client. A registered user's
  remembered listener is forgotten as well (a listener the user saw end does not
  come back at the next login); one that fails while the user is offline is
  skipped at login and stays stored, as above. A listener that only a temporary
  access token allowed therefore ends with the message that carried the token.

## Compatibility with Murmur

Handlers follow `Messages.cpp` and `Server.cpp`: the same permission checks in the
same order, the same `PermissionDenied` types and `Reject` reasons, the same
initial state on login (CryptSetup, CodecVersion, the channel tree and links, user
states, ServerSync, ServerConfig, SuggestConfig), the ACL engine of `ACL.cpp` and
`Group.cpp` (ordered evaluation, Traverse/Write abort, root-only permissions,
Write's implications, meta groups `all auth strong in out none sub,a,b,c`, the
`~ ! # $` prefixes), Murmur's default root ACLs, temporary channels counted down
from `i32::MAX`, text limits in UTF-16 units with the `<img>` exception, the
leaky bucket for `messagelimit`/`messageburst`, and blobs announced by hash from
128 bytes on. The client test suites run unchanged against stock Murmur and
`nuschld` (see Tests).

Deliberate differences:

- **Listeners end when Listen goes** (see "Listeners follow Listen" above);
  Murmur keeps them, so on Murmur a listener keeps hearing a channel its user
  may no longer listen to.
- **Ghost takeover.** An unregistered name in use is only taken over by the same
  certificate (or, both without certificates, the same address). Murmur lets any
  connection from the same address take it.
- **Certificates of registered users.** A password login from a new certificate
  does not replace the stored one (Murmur's behaviour lets a leaked password take
  the account over for good); an account without a certificate gets the first one.
- **Registration ids are never reused**, even after an account is deleted.
- **Session ids** count up and wrap only after 2³¹ sessions.
- **Server-browser pings** are answered with at most the request's size by
  default (`allow_ping_amplification = false`), so the server cannot amplify
  spoofed traffic; stock server lists then show no user count. With the option on,
  answers are at most twice the request (stock's 12 → 24 bytes), rate-limited per
  address and globally; protobuf requests under 12 bytes are never answered.
- **Access tokens** are capped (32 of up to 128 bytes) and kept only as SHA-256
  digests, compared in constant time.
- **Limits Murmur does not have**: per-type frame limits from the header (before
  authentication only `Version` ≤ 4 KiB, `Authenticate` ≤ 16 KiB and `Ping`
  ≤ 256 B; anything else closes the connection), element caps after decoding
  (ACL entries, groups, ban entries, voice targets, blob requests), a flood bucket
  for all control messages and a query bucket for permission and blob queries,
  per-address and pending-connection caps, and a bounded argon2id queue (a busy
  server answers `AuthenticatorFail`, "try again later").
- **Temporary channels per user and the whisper fan-out budget.** Fixed caps cost
  availability too (a large event, a legitimate shout to a big tree), so these are
  generous by default, configurable (0 turns them off), and fail only the request
  that exceeds them, with a reason, never the connection; resource limits on the
  process (`LimitNOFILE` in the NixOS unit; `MemoryMax`, `CPUQuota` or traffic
  shaping where the operator sets them) stay the main defence. The review suggested
  three temporary channels; 16 leaves room for people who keep a few rooms open for
  others. `temporary_channels_per_user` (16) counts the temporary
  channels one account or certificate has created that still exist, so nobody fills
  `channel_count_limit` alone; one more is refused with `PermissionDenied` (Text:
  "You already have 16 temporary channels…"). `whisper_fanout_per_second`
  (500,000, a second's worth at once) is shared by all whisper and shout targets:
  a target can reach a whole tree, so a few speakers could otherwise make the
  server send more than its link carries. A packet over the budget reaches nobody
  rather than an arbitrary part of the target (to the listeners it is packet loss),
  the speaker is told why at most every ten seconds, and the drops are counted.
  Channel voice is not counted; the per-user `max_bandwidth` bounds what each
  speaker sends in.
- **Clients older than 1.2.2** are refused with `Reject` (WrongVersion, "This
  server needs Mumble 1.2.2 or later"). Murmur special-cases them (comments,
  descriptions and avatars inline, since they cannot fetch blobs by hash), and they
  have no Opus, so on this Opus-only server they could neither talk nor hear;
  turning them away with the reason is clearer than half a session. A client that
  reports no version is admitted, as Murmur does. 1.2.2 and 1.2.3 (no Opus either)
  are admitted and told about the codec, as before.
- Not implemented: Ice/gRPC, external authenticators, e-mail login (also not
  with strong certificates, see "Strong certificates"), recording notices for
  clients older than 1.2.3, the pre-1.4 Opus/CELT codec switching.

## Security measures

Passwords are argon2id (19 MiB, two passes by default) on the blocking pool behind
a semaphore of two with at most 32 queued; the server password and tokens are
compared as digests in constant time. Password guesses are limited per account:
after ten failures (one more allowed per second) logins to that account are
answered with `AuthenticatorFail` ("try again later"). The TLS verifier accepts any client
certificate but rustls still checks the handshake signature, so presenting
someone else's certificate fails (tested, also for a certificate a configured
authority issued). TLS session resumption is off, TLS 1.2
can be refused (`minimum_tls_version`). Debug
output of protocol messages redacts passwords, tokens, keys, comments and
message bodies; logs contain user names, session ids and addresses, never
contents. With `obfuscate = true` (murmur.ini's name) every logged client address
is replaced by `<<token>>`, the first 64 bits of an HMAC-SHA256 under a key drawn at
startup and kept only in memory: one address keeps its token while the process runs
(lines of one client, or an abusive address, can still be correlated), tokens change
with every start, and without the key they cannot be mapped back, not even by
trying every IPv4 address (Murmur hashes under a 32-bit random number, which such a
search undoes). The server's own listening addresses are logged as configured;
`UserStats` still shows addresses to users allowed to see them, as in Murmur. The
database file is created 0600 in a 0700 directory. Handshake (TLS
within `handshake_timeout`, authentication within 1.5×) and idle timeouts,
autoban (Murmur's 10 attempts in 120 s → 300 s) and IP and certificate bans are
checked before a connection gets anywhere. The accept loop backs off on
descriptor exhaustion.

## The Nuschl extension

- **Negotiation.** `tls.rs` selects ALPN `nuschl/1` (and accepts `mumble`); the
  connection's `Negotiation` (extension.rs) accepts the client's hello once before
  `Authenticate` and freezes the capabilities; ALPN without a hello closes the
  connection before the credentials are decoded, a hello without ALPN is ignored.
  `nuschl/1` connections get a `Version` with the server's hello and policy. The
  client's hello carries its version; `Negotiation::refusal` turns away a Nuschl
  client below `minimum_client` from `minimum_client_from` on, before the
  credentials are decoded.
- **Dispatch.** `State::handle` passes frames in 0x4E00–0x4EFF to
  `State::extension_frame` (chat.rs), which processes them only for connections that
  negotiated the needed capability; caps per type come from the header
  (`nuschl::client_limit`) and again after decoding; at most 16 requests in flight.
- **Conversations** (conversations.rs): channels' `conversation_id`, direct
  conversations per pair of accounts, and archives of deleted permanent channels
  (SuperUser only); temporary channels' history is purged with them. Registered users
  have immutable 128-bit account ids. `can_read`, `can_write`, visible ranges,
  subscriptions, `since_access` intervals and read markers live in the state;
  `authz_changed` records each trigger and `State::finish` re-evaluates before the
  lock is released.
- **Writes** are assigned their `seq` under the lock and queued to the history writer
  (history.rs), which stores them in order and then, under the lock, acknowledges,
  fans out to current readers and sends stock clients their copy (made with
  `nuschl-legacy-html`, checked by Murmur's text rules). Stock senders' channel
  messages are stored as `legacy_html` (`State::text_message`).
- **Pages** go to the history reader through a bounded queue; the result is clipped
  again if `authz_epoch` moved and dropped if `can_read` no longer holds.
- **Page limits** (page_limits.rs): besides each connection's buckets, a token
  bucket per network (IPv4 /24, IPv6 /56) for unregistered users and per account
  for registered ones, for pages and for their bytes (charged once read, so a large
  page puts the bucket in debt). A /24 because IPv4 addresses are shared and one
  operator's hosts usually sit together; a /56 because residential IPv6 customers
  get a /56 or /48, so a /64 would hand one customer hundreds of budgets, while a
  /48 would lump several customers together. Exceeding a bucket fails that page with
  `rate_limited` and the milliseconds until it refills; buckets are bounded in
  number (idle ones go first, and beyond 65,536 they are forgotten, failing open to
  the per-connection limits).
- **Retention** (retention.rs, docs/extension.md §6): each conversation keeps its
  own retention (`history_retention`, set by `ChannelSettings` with Write on the
  channel and kept by a deleted channel's archive) or the server default, capped by
  `max_retention_days`. A shortening needs `confirm_expiry` and applies to existing
  messages. Every hour a task on its own storage worker tombstones expired rows,
  conversation by conversation in batches of 1,000, so neither chat writes nor
  pages queue behind it and no single statement holds SQLite's write lock for long;
  pages hide content past the retention immediately. What may expire is one
  predicate (`nuschl_ipc::server::expirable` and the worker's `expirable!` SQL),
  the hook where records the server must keep (MLS commits) are exempted. A
  live-only ("none") conversation stores no rows: its messages are acknowledged and
  delivered, and only its highest `seq` is recorded (`history_heads`). Expired and
  erased text is overwritten in SQLite (`secure_delete`); **PostgreSQL** keeps the
  old row versions until VACUUM reclaims them (autovacuum does so eventually; run
  `VACUUM history_messages` after an erasure or a retention change to be sure), and
  backups keep whatever they captured.
- **Key transparency** (kt.rs): `kt/1` requests are authorised under the lock (own
  account for publications, registered targets for lookups, the owner and contacts
  for histories, per-type rate limits) and queued to one task, which owns nuschl-kt's
  `AkdDirectory` on the `kt_records` table through a fourth storage worker. It checks
  each publication (the root's device-signing certificate, the devices, revocations
  and the identity's statement for this account and log), keeps the record behind
  each binding beside the log, publishes waiting bindings every `publish_interval`
  and re-signs the head every `refresh_interval`.

- **MLS delivery service** (mls.rs): `mls/1` requests are authorised through
  authz.rs under the lock and processed by one task through nuschl-e2ee (`PublicGroup` per
  group, structural checks before OpenMLS, `catch_unwind`): the key package directory,
  epoch fencing with `STALE_EPOCH`, commits and application messages as rows of the
  conversation's `seq` stream, Welcome mailboxes with push, join info; state in the
  key-value table under uppercase prefixes. A conversation with a group refuses
  plaintext. Details in docs/e2ee.md "Phase 4 prototype".
- **Share links** (share.rs, `share/1`, docs/web.md): random 16-byte tokens that open
  a channel, optionally with an access token for a private channel and an expiry,
  issued, resolved and revoked under the lock and kept in the key-value table under
  `S`. `[share_links] web_host` names the web host (`services.nuschl-web`) the
  server's links point at.

## Tests

| Command | What it covers |
| --- | --- |
| `cargo test -p nuschld` | authorisation (authz.rs: every action against seven kinds of actor, the three decisions that were made two ways, and a guard that no permission check lives elsewhere), strong certificates (chains to a configured authority, through intermediates, expired, other authorities, self-signed; and over loopback with `UserStats`), ACL engine (Murmur's cases), leaky bucket and bandwidth record, text limits, config, time format, argon2id; a fuzzing harness over the state (with Nuschl connections); loopback integration tests: login and initial state, every reject reason, a certificate without its key, oversized pre-authentication frames, channels, text, ACL denial, kick, temporary channels, voice over the tunnel and encrypted UDP to protobuf and legacy clients, whisper isolation, self-mute, restart persistence, hostile frames; the Murmur import (tests/fixtures/murmur-1.5.json: both password formats over loopback, the upgrade to argon2id, malformed rows reported, a non-empty server refused, `--replace-existing`, the ACLs in effect), listeners kept across reconnects and restarts, listeners that lose Listen (an ACL edit, a temporary token, unregistration) ended for stock and Nuschl clients with live delivery and pages stopped and others left alone, certificate reload (a broken one keeps the old) and `minimum_tls_version`, the temporary-channel cap and the whisper budget, clients before 1.2.2, the statistics counters; tests/obfuscation.rs: no client address in the log with `obfuscate` |
| `cargo test -p nuschld --test admin` | `nuschld admin` on a stopped server's state: users (list, show, rename, set a password, remove with its ACL entries), channel retention (refused without `--yes` when it removes messages), bans, exporting a channel (0600, nothing past the retention), erasing a user |
| `cargo test -p nuschld --test extension` | the extension between `nuschl-session` and `nuschld` on loopback: negotiation and every mismatch, downgrades against fake servers, sending and the stock copy, idempotent retries and conflicts across restarts, every history anchor and the byte cap, uniform errors, guests, revocation (subscriptions, fan-out, pages in flight), `since_access` intervals, edits and deletions, direct conversations and deleted accounts, read markers, typing, channel lifecycle, notices for stock clients, the operator's erase, the session's durable sends and resends, the minimum-version policy (an old session refused with `Outdated` and never retried, a hello without a version refused, stock clients admitted, admitted and told the date before it, a recommendation) |
| `cargo test -p nuschld --test mls` | the MLS delivery service with raw protocol clients and real identities: a direct conversation end to end, idempotent resends, stale epochs, hostile key packages, strangers, plaintext refusal, a restart |
| `cargo test -p nuschl-protocol` | OCB2 role constructors, relay parse/encode, info-ping limits, per-type frame limits, redacted `Debug` |
| `cargo test -p nuschl-storage server` | the server schema on SQLite, reopening, cascades, the registration high mark; history pages, idempotency, edits, tombstones, read markers, intervals, erase, retention batches and live-only heads; the key-transparency table; channel listeners and their cascades |
| `cargo test -p nuschl-storage murmur` | the Murmur reader on Murmur 1.5's own schema (tests/fixtures/murmur-1.5.sql, which also produces the import test's input) and 1.6's layout, malformed rows as notes, other virtual servers, files that are not Murmur databases or are damaged |
| `just test-postgres` (`cargo test -p nuschl-storage-worker --test postgres`) | the storage worker as `nuschld` drives it, on SQLite and on a throwaway PostgreSQL from nixpkgs (`initdb` in a temporary directory, a unix socket only, no TCP): migrations, users, history pages, idempotency, edits, deletions, retention, live-only heads, erasure, purging, a second worker on the same database, and the operator lock (one holder; the others answer `Held` without opening the database). Without `initdb`/`postgres` on `PATH` (or in `NUSCHL_POSTGRES_BIN`) the PostgreSQL half says SKIPPED and passes |
| `cargo test -p nuschl-crypto-worker` | key transparency end to end: nuschld, the session and the crypto worker in its sandbox (publish and look up, TOFU and verification, key changes, the self-monitoring alarm, a forked and a replaced log), and the worker against a malicious directory in process |
| `python3 tests/with_local_nuschld.py <bins> tests/<suite>.py <bins> --port PORT …` | `mumble_client.py`, `udp_voice.py`, `protocol_features.py` against nuschld; `with_local_murmur.py` runs the same scripts against stock Murmur |
| `nix build .#checks.x86_64-linux.vm-server --cores 2` | the module with SQLite and PostgreSQL, restarts, the client suites (with loss and blocked UDP), key transparency (`tests/key_transparency.py`, keys as credentials and generated), and the stock Mumble GUI (login with a Nuschl-exported certificate, voice both ways); the statistics line, SIGHUP, and `services.nuschl.importMurmur` importing the Murmur fixture into a fresh database, then logins with its PBKDF2 and SHA-1 passwords (tests/murmur_import.py) |
| `just fuzz server_messages [seconds]` (SECURITY.md, "Fuzzing") | also `control_frames`, `relay`, `crypt`, `extension_messages`, `kt_messages` |
