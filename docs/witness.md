# Key-transparency witnesses

Built 2026-11-11 as the user decided (option B of
[the key-transparency review](reviews/2026-11-11-key-transparency.md), §5 and
"Answers"): a small fixed set of witnesses that every client checks, led by one
project-run auditor, `nuschel-witness`. **It is off until the user deploys the
auditor**: `nuschel_kt::DEFAULT_WITNESSES` is empty, so clients require nothing and
behave as before, and servers submit nowhere. Turning it on is one entry in that
constant (see "Switching it on").

## What it gives

Without witnesses a server can show two people two different key directories
(review G1): each view verifies, and nothing outside the server is consulted. With
witnesses, a client accepts a server's view as *witnessed* only if the auditor has
cosigned it within the last 24 hours. The auditor cosigns one history per server and
checks that every step of it is an append-only update of the directory, so a split
view needs the auditor's collusion, and a frozen view lasts at most a day.

## Design

- **Each server collects its own cosignatures.** `nuscheld` keeps an *epoch log*, an
  RFC 6962 Merkle tree whose leaf `i` is its akd epoch `i`, signs C2SP checkpoints of
  it, submits the new epochs to the witnesses, and attaches the newest cosigned
  checkpoint with the head's inclusion proof to the heads it serves. Clients never
  contact a witness and never relay anything between servers: no witness learns who
  uses which server, and no server learns which other servers a user is on. (Option
  A, the user's other servers witnessing each other, and option C, arbitrary
  witnesses, are not built: quorums would not intersect, review §5.3.)
- **The auditor checks akd, not only the tree.** A submission carries akd's
  append-only proof from the auditor's newest epoch to the server's; the auditor
  rebuilds the leaves from it, appends them to its own copy of the frontier and
  cosigns only if that gives the server's checkpoint. It starts every log at epoch 0,
  which must be akd's empty directory, so nothing in a log escapes its checks (no
  trust on first use of log state). Clients keep auditing akd themselves between the
  heads they rely on (review §5.6).
- **One log per origin.** The origin is `<host>:<port>/nuschel-kt/v1`, the server as
  clients reach it (the session's sticky key). The auditor enrols an origin once: it
  connects to `host:port` as a Nuschel client and requires the server's hello to
  announce the submitted log descriptor (`kt_log`), as ACME checks a name. Afterwards
  it refuses another descriptor for that origin (logged as possible evidence). A
  client accepts a checkpoint only for the origin it connects to, so a server cannot
  show a victim a second log under another name.
- **Cosignatures in ML-DSA-44 from the start** (the user's decision). C2SP defines
  two timestamped checkpoint cosignatures: Ed25519 (`cosignature/v1`, signature type
  0x04) and ML-DSA-44 (type 0x06), which signs the binary `CosignedSubtree`
  structure (`"subtree/v1\n\0"`, cosigner name, timestamp, origin, start 0, end =
  tree size, root) with FIPS 204 and an empty context. Nuschel uses 0x06 only, with
  key ids `SHA-256(name ‖ "\n" ‖ 0x06 ‖ public key)[:4]` and C2SP verifier keys
  (`<name>+<key id>+<base64(0x06 ‖ key)>`), read from the C2SP editor's copies of
  2026-11. Checkpoints, signed notes and the log's own Ed25519 signature (type 0x01)
  are C2SP's, so the formats stay interoperable; the submission API (`add-epochs`)
  is Nuschel's, because a C2SP witness checks Merkle consistency, not akd. A
  cosignature line is 2,432 bytes (4-byte key id, 8-byte timestamp, 2,420-byte
  signature) against 76 for Ed25519. ML-DSA-44 comes from libcrux, which
  nuschel-identity already uses for ML-DSA-65.
- **Freshness comes from the witnesses' clocks** (review findings G3 and the clamp in
  §4.10). A head's own `issued_at` is signed by the server, which re-signs its current
  epoch hourly; a server could re-sign an old epoch with a fresh time to freeze a
  client. With witnesses required, a head counts as fresh only through the
  cosignature's timestamp, and a head must be the newest leaf of the checkpoint it
  comes with: an old head re-signed with a fresh time either comes with the newer
  checkpoint (refused: not the newest leaf) or with its old one, whose cosignature
  ages out, because the auditor never cosigns a smaller size again. The server's
  clamp (`issued_at` never decreases) made one forward clock jump an outage until
  real time caught up; the directory now signs again any head more than an hour in
  the future (which no client with a correct clock can have accepted), and witnessed
  clients do not consult `issued_at` at all. Epoch leaves carry no time.
- **User names and registration numbers are in the log** (review G4). Every
  registered user's `reg:<number>` and `name:<lower-case name>` labels name their
  account; a rename releases the old name (format 4, a visible entry). A lookup by
  number carries both proofs at the same head, and the client requires the number's
  label, and the name label of the name the server shows, to name the account it got.
  Owners monitor their own number and name like their binding. Contacts stay keyed by
  identity after first contact.

## What clients do

| Situation | Result |
| --- | --- |
| No witness required (today) | As before; a checkpoint the server attaches is still checked (log signature, origin, inclusion), and a bad one fails the head |
| Required witnesses cosigned the head's checkpoint within the bound | Witnessed |
| The head is newer than the newest cosigned checkpoint | Pending; after ten minutes: unavailable |
| Cosignature missing, stale (older than 24 h), from the future, or the server lacks `kt-witness/1` | **"Automatic verification unavailable"**: contacts show it (amber), first contact proceeds as "not verified" with the key pinned, messages keep using pinned keys, and contacts' new devices are not added to existing end-to-end conversations |
| A checkpoint that is not the log's, for another origin, with a wrong inclusion proof, or newer than the head presented as newest | The head fails (retried; e2ee.md §5.5) |
| `reg:` or `name:` label naming another account, or missing | The lookup fails |
| Own number or name bound to another account | The blocking own-key alarm |

The 24-hour bound and the failure policy are the review's proposals, which the user
has not confirmed; both are settings.

## Configuration

**Clients** (`key_transparency` in settings.json or managed.json, or Home Manager's
`programs.nuschel.keyTransparency`):

```json
"key_transparency": {
  "default_witnesses": true,
  "witnesses": [ { "key": "witness.example.org/nuschel-kt+5d32ee69+BuajLAZT…", "enabled": true } ],
  "required": null,
  "max_age_hours": 24,
  "block_new_devices": true
}
```

`required` is how many of the enabled witnesses must have cosigned (unset: all);
clients that require `k` of the same `n` witnesses are safe from split views only if
`2k > n`. An entry with `enabled: false` turns a built-in witness off. At most 16
witnesses; `max_age_hours` from 1 to 168. A managed list replaces the user's and is
shown locked. The crypto worker parses the keys and ignores invalid ones.
`nuschel-cli --kt-witness <vkey> --kt-witness-max-age-hours N` sets the same.

```nix
programs.nuschel.keyTransparency = {
  witnesses = [ { key = "witness.example.org/nuschel-kt+5d32ee69+BuajLAZT…"; } ];
  maxAgeHours = 24;
};
```

**Servers** (`[kt]` in nuscheld.toml; NixOS `services.nuschel.kt.*`):

```toml
[kt]
origins = ["chat.example.org:64738"]  # the names clients use; NixOS: the host's FQDN
default_witnesses = true              # submit to DEFAULT_WITNESSES
witness_interval = 60                 # seconds between submissions of new epochs
witness_refresh_interval = 3600       # resubmit an unchanged checkpoint

[[kt.witnesses]]                      # more witnesses
key = "witness.example.org/nuschel-kt+5d32ee69+BuajLAZT…"
url = "https://witness.example.org"
```

Without an origin nothing is submitted, and clients that require witnesses see the
server as unavailable. A server reachable under several names lists each.

## Running the auditor

On NixOS (`inputs.nuschel.nixosModules.witness`):

```nix
services.nuschel-witness = {
  enable = true;
  virtualHost = "witness.example.org";  # nginx in front, servers submit to https://…
  enableACME = true;
  openFirewall = true;                  # 80 and 443
};
security.acme = { acceptTerms = true; defaults.email = "admin@example.org"; };
```

Defaults: cosigner name `<virtualHost>/nuschel-kt`, the key generated once in
`/var/lib/nuschel-witness/witness.key` (or `keyFile`, loaded with `LoadCredential`;
never in the Nix store), plain HTTP on 127.0.0.1:8473 behind nginx, new logs
accepted, at most 10,000 logs, 60 submissions per log and minute, 10 enrolments per
minute, 16 MiB requests, private addresses not enrolled (`enrolPrivateAddresses`),
`DynamicUser` and the same systemd sandbox as nuscheld. **Back up the key**: clients
trust it, and a new key is a new witness.

The verifier key is in the journal (`journalctl -u nuschel-witness | grep vkey`), at
`https://<virtualHost>/nuschel-kt/v1/vkey`, and printed by
`nuschel-witness vkey --name <name> --key-file <file>`.

Elsewhere: `nuschel-witness serve --name witness.example.org/nuschel-kt
--state-directory /var/lib/nuschel-witness` behind any TLS reverse proxy.

HTTP API (plain HTTP/1.1, `Connection: close`):

| Request | Answer |
| --- | --- |
| `POST /nuschel-kt/v1/add-epochs`, body `AddEpochs` | 200 with the cosignature line; 409 with the auditor's size (`text/x.tlog.size`); 400 malformed, 403 not the log's signature or another log for the origin, 422 the epochs or checkpoint do not check out, 429 rate limited, 502/504 enrolment could not reach the server, 413 too large |
| `GET /nuschel-kt/v1/checkpoints/<hex SHA-256 of the origin>` | The newest checkpoint it cosigned, for monitors |
| `GET /nuschel-kt/v1/vkey`, `GET /` | Its verifier key |

The state is one file per origin (`logs/<hash>.state`: descriptor, size, frontier,
newest akd root, last timestamp, a few hundred bytes), written atomically before a
cosignature leaves.

## Switching it on

When the auditor runs:

1. Get its verifier key and URL.
2. Put them into `crates/kt/src/witness/policy.rs`:

   ```rust
   pub const DEFAULT_WITNESSES: &[DefaultWitness] = &[DefaultWitness {
       vkey: "nuschl.eu/nuschel-kt+…",
       url: "https://witness.nuschl.eu",
   }];
   ```

   and change the unit test that asserts the list is empty.
3. Release servers first (they start submitting; servers need `kt.origins`), then
   clients (they start requiring it). A server that is not yet witnessed shows as
   "Automatic verification unavailable" to new clients until it is.

## Formats

All in nuschel-kt's strict codec (docs/e2ee.md §5.6) or C2SP's text formats; the
`kt_witness` fuzz target covers every parser.

| Type | Message | |
| --- | --- | --- |
| 8 | `EpochLeaf`: log id, epoch, akd root | 74 bytes; leaf hash `SHA-256(0x00 ‖ leaf)` |
| 9 | `WitnessedHead`: a signed checkpoint note (≤ 64 KiB) and an inclusion proof (≤ 48 hashes) | In `KtStatusReply`, `KtLookupReply`, `KtHistoryReply` (`witnessed`) |
| 10 | `AddEpochs`: origin, descriptor, the auditor's size, an `AuditResponse`, the log-signed checkpoint | The auditor's submission |
| 11 | `LogState`: what the auditor keeps per origin | Its state file |

Checkpoint: `<origin>\n<size>\n<base64 root>\n`, no extension lines; notes with at
most 32 signature lines.

## Privacy

The auditor learns each server's origins, how many epochs it publishes and when, and
how many leaves each epoch changes; nothing about labels or values. Servers relay
cosignatures, so the auditor never sees clients. Name and number labels are hidden
by the VRF like every label; a rename is visible to those who look up the old name.

## Costs

Per reply: one checkpoint (about 2.5 KB per ML-DSA-44 cosignature) and
⌈log₂ epochs⌉ hashes. Per server and witness: a submission at most every minute
while epochs are new and hourly otherwise, carrying akd's per-epoch proofs (about
1–2 KB per changed account; the first submission carries the whole history from
epoch 0). The auditor verifies about a millisecond per changed account.

## Tests

- `crates/kt`: RFC 6962 vectors, every inclusion proof and mutation, C2SP's
  signed-note example, ML-DSA-44 cosignatures, the auditor's refusals (another log,
  forged checkpoints, tampered proofs, conflicts), a fork cosigned by nobody, stale
  and re-signed heads, pending heads, the clock rewind, a large history in steps.
- `apps/witness`: enrolment against a fake server's hello (and one announcing
  another log), restarts, private addresses, malformed and oversized bodies.
- `apps/crypto-worker/tests/witness.rs`: the client rules against a malicious
  server, names and numbers, owners' monitoring; `tests/kt.rs`: nuscheld,
  nuschel-witness and real sessions on loopback.
- `vm-server`: a witness node behind nginx, the module's server options,
  `tests/kt_witness.py`.

## Open

- The desktop has no settings page for witnesses yet (settings.json, managed.json
  and Home Manager work); it shows "Automatic verification unavailable" through the
  existing contact state, and no "verified automatically" state.
- History proofs still grow with every version (review G7).
- Only one cosigner algorithm (ML-DSA-44); Ed25519 C2SP witnesses could be added as
  type 0x04 if an independent witness only speaks that.
- The auditor's enrolment check trusts what the network shows at `host:port`, like
  ACME's HTTP challenge.
- Checkpoint equivocation evidence (two log-signed checkpoints of one size) is not
  collected by clients; the auditor refuses the second lineage instead.
