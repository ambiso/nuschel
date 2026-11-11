# Key transparency review (2026-11-11)

Research and writing only; no code was changed. The question was: how does Nuschel's key
transparency (KT) work, is it a faithful implementation of what Signal and WhatsApp do,
and what should change. The user has since decided that **akd stays**; KEYTRANS appears
here only as a comparison. The user's follow-up, "how does this make sense in federation?
Could every Nuschel server become an auditor?", is answered in §5, the central section.

Method: primary sources were downloaded and read as text (blog posts as HTML, papers as
PDF through `pdftotext`, specifications and code from their repositories). §1 lists every
source with its version and how much of it was read. Claims about Signal are marked
**[code]** when checked in Signal's source and **[blog]** when they rest on a post only.
Nuschel's side was read from `main` at 46dbc8e. Proof sizes in §5.8 were measured with a
temporary test harness in this workspace (a copy of `crates/kt/tests/perf.rs` with other
account counts), which was deleted afterwards and is not part of this commit.

Severity: **Critical** = the property the feature exists for does not hold against the
attacker it is meant for (here: a malicious or compelled server); **High** = a gap that
lets that attacker succeed in a common case, or a guarantee the docs claim but the code
does not give; **Medium** = a real gap with a clear fix; **Low** = hardening, cost or
clarity.

---

## 0. Summary

**Is it faithful to Signal or WhatsApp?** No. It is a hybrid, and it lacks the one part
that makes both deployments work. The data structure is WhatsApp's (akd, from the
SEEMless/Parakeet line); the client checks resemble Signal's (self-monitoring, contact
checks, automatic failure handling); head gossip resembles Apple's but is not
transported; auditors are optional and none exist. Signal's clients refuse a tree head
unless **every one of three auditors** (one run by Signal, one by Cloudflare, one by
Trail of Bits) has signed a consistent head within the last seven days **[code]**.
WhatsApp's directory is cosigned and audited by Cloudflare (whether WhatsApp clients
require that signature is not documented in anything I read). Nuschel has nothing
outside the server's control that a client checks. The earlier design work read blog
posts and the KEYTRANS drafts but no papers; this review read the papers and code listed
in §1, and the gap it found is in none of the papers' constructions but in the part
they all assume: a channel the server cannot split (SoK §3.1). Against a malicious server, which is the
attacker KT exists for, Nuschel's KT today is a server-signed directory plus
self-monitoring that the server can fork. The IETF architecture draft says this
configuration is "not secure" (draft-ietf-keytrans-architecture-11 §3.3).

The akd construction itself is fine for Nuschel's scale. The fixes are around it.

| # | Sev | Finding |
|---|-----|---------|
| G1 | Critical | No split-view resistance: no witness, auditor or other channel outside the server's control is required by clients (§4, §5) |
| G2 | High | Head gossip is not transported, and in-band gossip inside MLS cannot detect the attack that matters (a targeted full MITM), because the impostor relays the gossip (§5.2) |
| G3 | High | The head freshness bound (25 h) is not a bound: `issued_at` is asserted by the server, which re-signs old epochs with new times by design (§4) |
| G4 | High | Names and registration numbers, the identifiers people use, are mapped to account ids outside KT: only `acct:` labels are published. KEYTRANS §9 calls this "equivalent to not using KT at all" (§4) |
| G5 | High | Federation: per-server logs with no cross-server witnessing; a client's own set of servers cannot replace a common witness set (quorum intersection, §5.3) |
| G6 | Medium | First log state and log key are trusted on first use; combined with G1 a new client can be given a private fork (§4) |
| G7 | Medium | History proofs grow by about 4.2 to 5.3 KB per version and are fetched in full on every check; every device or certificate change adds a version (§5.8) |
| G8 | Low | Own refinements are sound (account statements, roster commitment, stale-signing evidence), with caveats in §4.10 |
| G9 | Low | No absence proofs (handled correctly as failures), but the desktop presents the server's unauthenticated "not found" as fact |
| G10 | Low | Doc errors in e2ee.md §4 and §5, including the auditor claim (§9) |

**Recommendation** (§7), with the user's decisions of 2026-11-11 built in: keep akd, and
make **witnessing mandatory once the project auditor is deployed**. Each server keeps a
small append-only C2SP transparency log of its akd epoch heads (a "log of map roots",
the shape KEYTRANS has natively). One **project-run auditor** (`nuschel-witness`, a
small separate binary with a NixOS module) checks each akd transition and cosigns the
server's checkpoints; clients require its cosignature, no older than a freshness bound,
which gives split-view resistance and a freshness bound the server cannot forge. Its key
ships in the client and in `nuscheld`'s defaults, empty until the user deploys it:
until then clients behave exactly as today. Users and administrators can add, remove or
disable witnesses in the client's settings and through `programs.nuschel` (managed.json).
The **other servers a user is connected to** witness each other's logs for that user
(chosen by the client, never by the witnessed server); they add protection where users
share servers but cannot replace the common auditor (§5.3). Clients keep auditing akd's
append-only proofs themselves, which is cheap at 100 to 10,000 accounts. Publish
`name:` and registration-number labels. Do not ship gossip in MLS as a defence.

---

## 1. Sources

All retrieved 2026-11-11. The IACR ePrint server answered automated downloads with a
Cloudflare challenge, so ePrint papers were taken from Wayback Machine snapshots of the
ePrint PDF (date given); a later ePrint revision may differ.

### Read in full

| Source | Version / date | Notes |
| --- | --- | --- |
| Signal, "Introducing Automatic Key Verification", https://signal.org/blog/automatic-key-verification/ | K. Yen, 2026-08-11 | |
| Trail of Bits, "How Trail of Bits helps verify the integrity of your Signal chats", https://blog.trailofbits.com/2026/08/11/how-trail-of-bits-helps-verify-the-integrity-of-your-signal-chats/ | T. Hess, 2026-08-11 | States three auditors and the seven-day window; publishes ToB's auditor key |
| libsignal `rust/keytrans/src/lib.rs` and `verify.rs` lines 1–340 (full tree head verification); `rust/net/src/env.rs` (KT configuration) | github.com/signalapp/libsignal @ 0abcfd40 (2026-11-10), AGPL-3.0-only | Client checks **[code]** |
| draft-ietf-keytrans-architecture-11 | 2026-11-01; datatracker: IESG Evaluation | |
| C2SP `signed-note.md`, `tlog-checkpoint.md`, `tlog-cosignature.md`, `tlog-witness.md`, `tlog-policy.md` | github.com/C2SP/C2SP @ 2bc1f467 (2026-11-03) | Editor's copies |
| N. Möller, "Notes on the implication of dishonest witnesses" (Sigsum), https://git.glasklar.is/sigsum/project/documentation/-/raw/main/archive/2023-11-byzantine-witnesses.pdf | 2023-12-04 | Threshold analysis; cited by tlog-witness |
| Witness Network, https://witness-network.org/ (home, participate, FAQs, witness tables, log lists) | 2026-11-11 | Production lists "not available yet" |
| Parakeet, ePrint 2023/081, full version | Wayback 2024-12-28 | Body §I–§VIII read; benchmarks skimmed; appendices not read |
| Meta, "Deploying key transparency at WhatsApp", https://engineering.fb.com/2023/04/13/security/whatsapp-key-transparency/ | 2023-04-13 | |
| Meta, "Key Transparency Comes to Messenger", https://engineering.fb.com/2025/11/20/security/key-transparency-comes-to-messenger/ | 2025-11-20 | |
| Cloudflare, "Cloudflare helps verify the security of end-to-end encrypted messages by auditing key transparency for WhatsApp", https://blog.cloudflare.com/key-transparency/ | Meunier, Galicer (2024) | |
| Cloudflare press release on Signal, https://www.cloudflare.com/press/press-releases/2026/signal-taps-cloudflare-to-help-protect-the-privacy-of-hundreds-of-millions-of-users-through-key-transparency/ | 2026-08-11 | |
| Cloudflare developer docs, `key-transparency/` and `key-transparency/monitor-the-auditor/` | updated 2026-10-02 and 2026-05-05 | |
| RWC 2025 slides, "Auditing Key Transparency" (Galicer, Meunier, Lewi), https://iacr.org/submit/files/slides/2025/rwc/rwc2025/85/85_slides.pdf | 2025 | Text only; figures not visible in extraction |
| akd `README.md`, `CHANGELOG.md`, crate documentation in `akd/src/lib.rs` | github.com/facebook/akd @ 43a60ccf (2026-08-12), 0.13.0 | |
| Apple, "Advancing iMessage security: iMessage Contact Key Verification", https://security.apple.com/blog/imessage-contact-key-verification/ | 2023-10-27 | |
| Google, "Android System Key Verifier" (support.google.com/android/answer/15669061) and "Enable private communication with Key Verifier" (developer.android.com/privacy-and-security/key-verifier) | page dated 2026-03-23 | Short pages |
| IETF 125 slides, "Key Transparency Verification Update" (F. Linker) | 2026-03-18 | One page |
| EUPL-1.2, Article 5 and Appendix | repository `LICENSE` | |

### Read in part

| Source | What was read |
| --- | --- |
| draft-ietf-keytrans-protocol-06 (2026-11-02, WG draft) | §3 (start), §6.1, §7.1, §10, §11.1–§11.4, §15, §16 |
| signalapp/key-transparency-server @ 2eb0ead9 (2026-10-21), AGPL-3.0 | README; `cmd/kt-server/distinguished.go`; grep of `kt_handler.go`, `cmd/internal/config/config.go` |
| libsignal `rust/net/chat/src/api/keytrans/monitor_and_search.rs` | Constants and outline only |
| CONIKS, ePrint 2014/1004 (Wayback 2024-12-28) | §4.1.4 (auditing), §4.2, Appendix B (equivocation detection, colluding auditors); rest skimmed |
| SEEMless, ePrint 2018/607 (Wayback 2024-12-28) | Introduction, VKD definitions (auditors, soundness) |
| SoK: On the Security Goals of Key Transparency Systems, ePrint 2024/1938 (Wayback 2026-01-06) | §1–§3.1 |
| OPTIKS, USENIX Security 2024 | Abstract, introduction, the username/UUID section |
| Consistency-or-Die, ePrint 2024/879 (Wayback 2025-12-13) | Abstract, introduction, system model (Sybil assumption) |
| Merkle², ePrint 2021/453 (Wayback 2023-12-03) | Abstract, introduction |
| Go, "Proposal: Secure the Public Go Module Ecosystem" (design 25530) | §Proxying, §Security (2019-04-24) |
| cloudflare/plexi @ 0108f7d2 (2025-04-16), Apache-2.0 | README, `plexi_core/src/lib.rs` outline |

### Abstracts or metadata only

PRISM (ePrint 2025/2026, Wayback 2026-05-27) and Aegon (ePrint 2026/1681, Wayback
2026-10-03): abstracts. IronDict (ePrint 2025/1580, USENIX Security 2026) and
Forget-me-not Trees (ePrint 2026/1276): search-result abstracts; not downloaded.
crates.io metadata for `signal-auditor` (Trail of Bits, AGPL-3.0-only), `plexi`
(Apache-2.0), and searches for KEYTRANS crates.

### Not read

The WhatsApp Key Transparency whitepaper (www.whatsapp.com answered HTTP 400, no archived
copy); ELEKTRA (known only through the SoK's quotations); Apple Platform Security guide;
the NCC Group audit of akd. Nothing found on federated or multi-provider KT beyond
CONIKS's multi-provider auditing (§4.1.4) and KEYTRANS architecture §5.3.

### Nuschel

docs/e2ee.md §2–§5, §10 (questions 3 and 9) and "Phase 3 as implemented";
docs/extension.md §3 and the `kt/1` implementation note;
docs/reviews/2026-11-11-server-security.md (it has no KT section; only D8 item 7 on
first contact). Code: `crates/kt/src/{lib,head,log,gossip,values,verify}.rs`,
`akd_backend/mod.rs` (heads, publish, audit), `apps/crypto-worker/src/worker.rs` (head
acceptance, gossip) and the `mls.rs` header, `apps/server/src/kt.rs` (module docs,
labels), `apps/desktop/src/identity.rs` (status texts), `crates/identity` (`AccountStatement`).

---

## 2. What Signal, WhatsApp and Apple actually do

### 2.1 Signal

- **Data structure.** A log tree whose entries commit to prefix trees, from "an earlier
  draft of the IETF key transparency protocol" with "refinements to fit Signal's use case"
  [blog]. The server is Go (`signalapp/key-transparency-server`), client verification is
  Rust (`libsignal-keytrans`), both AGPL-3.0 [code]. The server README cites CONIKS and
  Merkle² as background: this is the CONIKS → Merkle² → KEYTRANS line, not akd's.
- **Labels.** ACI (account identifier) → identity key, E.164 → ACI, username hash → ACI;
  the client stores monitoring data for exactly these three (`AccountData { aci, e164,
  username_hash }`) [code]. Contact verification needs the contact's phone number [blog].
- **Deployment mode and auditors.** `DeploymentMode::ThirdPartyAuditing` with three
  auditor keys in the production configuration (`KEYTRANS_AUDITOR_KEY_MATERIAL_PROD`,
  `rust/net/src/env.rs`) [code]. The third key, `7fe5d91d…cbbb6`, is the one Trail of
  Bits publishes as its auditor key. Trail of Bits: "Clients currently require
  signatures from each of three auditors: one operated by Signal, one operated by
  Cloudflare, and one operated by Trail of Bits." The code agrees: a head is rejected
  unless auditor tree heads are present "for all auditors" (`verify_full_tree_head`)
  [code]. Signal's own blog names only Cloudflare and Trail of Bits as "trusted
  third-party auditors" [blog].
- **What the client checks per head** [code, `verify.rs`]: the server's signature (one
  per auditor key; the signed data contains cipher suite, deployment mode, the signature
  and VRF keys, that auditor's key, tree size, timestamp and root); the server head's
  timestamp within ±24 h of the local clock; a consistency proof from the last head and
  from the last distinguished head; for each auditor: its head's timestamp at most 7
  days behind, its tree size not ahead of the server's and at most 10,000,000 entries
  behind, a consistency proof from the auditor's head to the server's, and the auditor's
  signature. A head with the same tree size as a known one but another timestamp is
  rejected ("tree size is the same but timestamps differ"): a tree size cannot be
  re-signed with a newer time.
- **Split-view bound.** "A fully malicious server may therefore maintain a split view of
  the system for at most one week before client applications start to display warning
  messages" (Trail of Bits). Auditors "commit to only ever signing one consistent lineage".
- **Distinguished entries.** Not the RMW algorithm of protocol -06 §6.1: the server
  updates a special label `distinguished` with the current time at a configured interval
  (`distinguished.go`) [code], and the client refreshes its distinguished head when it is
  older than two weeks (`MAX_DISTINGUISHED_TREE_AGE`) [code].
- **Monitoring and UX.** The app checks its own identifiers periodically; verifying a
  contact is started by the user with "Verify automatically" on the safety-number screen,
  and is unavailable for contacts found by username only [blog]. The feature can be
  turned off [blog].
- **Privacy.** Identifiers through a VRF, values under a keyed hash; lookups
  unauthenticated [blog]. The server has a fake-updates setting (`FakeUpdates` in
  `config.go`) [code, grep only], matching KEYTRANS architecture §8.2.

### 2.2 WhatsApp and Messenger

- **Data structure.** akd, "based on the original academic works on key transparency,
  starting with CONIKS and SEEMless, with extensions from … Parakeet"; pending changes
  are queued and published in batched epochs; an audit proof of each epoch goes to public
  storage (WhatsApp post). Messenger uses the same, with epochs about every 2 minutes and
  "hundreds of thousands of new keys added in each epoch" (Messenger post).
- **Auditing.** Cloudflare's auditor has two parts (Cloudflare blog): a *timestamping
  service* that receives each epoch as `{counter}/{previous}/{current}`, checks that
  epochs are consecutive and digests unique, timestamps and signs it (state per epoch is
  cleared after a month); and *tree validation*, which runs akd's
  `verify_consecutive_append_only` on every epoch in a container, asynchronously. The
  RWC 2025 slides name the two roles "witnessing" and "monitoring": "The witness provides
  a sync signature. … The monitor does an async signature. … They have the same format
  but not the same public key." Plexi (Apache-2.0) is the public verification client;
  the signed message is (cipher suite, namespace, timestamp, epoch, digest).
- **Do WhatsApp clients require Cloudflare's signature?** Not stated in any source I
  read. Cloudflare's docs say "The Log can then distribute these signatures to its
  end-users". Treat client enforcement as **unverified**.
- **Parakeet's consistency protocol** (paper §II, §V): 3f+1 witnesses, quorum 2f+1. Each
  witness checks that the epoch increments by one and that the append-only proof is
  valid before countersigning; witnesses keep only the latest commitment and need not
  talk to each other. Consistency follows from quorum intersection (§V-B). Optional
  extensions: witnesses sign their local time and clients take the median of 2f+1
  timestamps to bound staleness; an anti-censorship subprotocol where witnesses refuse
  to sign epochs that omit an update forwarded to them (§V-C). Parakeet's witness is a
  witness and auditor in one. The paper rejects client gossip: "there is a high risk that
  users do not eagerly gossip proofs or that the adversary launches targeted eclipse
  attacks" (§V).
- **Deletion.** "When a user deletes their account, we remove all of the public keys for
  that account, but the fact a key existed at a point in time is immutable" (WhatsApp
  post). Parakeet's compaction (§IV) tombstones entries older than a parameter at
  designated epochs and deletes them at later ones; users must check their history
  between the two.
- **Caveat from the SoK** (§1.1): SEEMless's proof needs a simulatable VRF, which
  WhatsApp's ECVRF-EDWARDS25519-SHA512-TAI cannot be, so "provable security cannot be
  claimed for their protocol on the basis of [SEEMless]" ("This does not necessarily
  mean that there is an attack"). The same applies to Nuschel, which uses the same VRF.
  The SoK also notes SEEMless alone does not give even weak consistency without frequent
  history checks (§3, Appendix E).

### 2.3 Apple Contact Key Verification

A CONIKS-style log-backed map indexed by a hash of a VRF output; Signed Mutation
Timestamps with a 48-hour maximum merge delay checked on the device; devices verify the
consistency of "critical append-only logs" themselves; an account key in iCloud Keychain
signs device keys; and "Messages also gossips log hashes — by including them in the
encrypted part of a small percentage of messages". At launch the auditing service was
internal, with "more details about our public auditing strategy in 2024". I found no
later public source on Apple's third-party auditing.

One difference matters for Nuschel (§5.2): iMessage encrypts to each device separately,
so a ghost device added by the server does not remove the real devices from the
conversation, and gossip between the real endpoints still flows. In MLS a ghost member
is visible to every member in the shared roster, so a working attack has to replace the
whole conversation, and then all gossip passes through the attacker.

### 2.4 Google Key Verifier

A system service for manual verification by QR code or number comparison, shared across
apps. The pages read describe no transparency log, gossip or auditors. It is not a KT
system and offers nothing for this question.

### 2.5 KEYTRANS, for comparison

- **Status.** Architecture -11 is in IESG Evaluation; protocol -06 is a WG draft. The
  only security analyses found: the SoK (architecture -03, protocol -00) and a verified
  "simplified greatest-version search" with clients at the same epoch (IETF 125 slides).
- **Fork detection** (architecture §3.3): needs "a trusted third party, anonymous
  communication with the transparency log, or peer-to-peer communication", and gossip is
  secure only if "gossiping users are reasonably expected to form a connected graph of
  all users". "Applications that do not provide their users with a trusted third party,
  anonymous communication, or peer-to-peer communication are not secure."
- **Thresholds** (§4): with several third parties, "the threshold for a valid signature
  needs to be at least a majority".
- **Configuration** (protocol -06 §11.2): one `auditor_public_key`, `max_auditor_lag`,
  `auditor_start_pos`, `max_ahead`, `max_behind`, the RMW and an optional maximum
  lifetime, all signed into every tree head. Signal's signed configuration predates this
  (no `max_ahead`/`max_behind`/RMW fields; several auditors).
- **Federation** (§5.3): each entity runs its own log; an entity "can act as an
  anonymizing proxy for its users when they query transparency logs run by other
  entities", but "should typically not attempt to 'mirror' or combine other transparency
  logs with its own".
- **Identifiers** (§9): if a username is translated to an internal id and "this mapping
  … is unauthenticated, the service operator can manipulate it … equivalent to not using
  KT at all". Secondary identities "should be a label in KT with the primary end-user
  identity as its associated value".

### 2.6 Literature relevant to federation

- **CONIKS** (§4.1.4) already proposed providers auditing each other: "CONIKS allows
  identity providers to facilitate auditing for their clients by acting as auditors of
  all CONIKS providers with which their users have been in communication"; clients
  compare a provider's head with the head held by auditors "chosen randomly"; an auditor
  stores under 1 kB per provider. Appendix B: if a fraction p of auditors collude and
  each user checks k random auditors, both fail with probability about
  ((p + (1−p)/2)²)^k; "If p = 0.1, then by doing 5 checks each, Alice and Bob will
  discover equivocation with 99.7% probability". Whistleblowing was left "for future
  work"; the SoK (§3.1) finds it still underexplored.
- **SEEMless** (§1): security needs "at least one honest auditor" per epoch pair, and
  assumes a bulletin board for heads. The SoK (§3.1) treats split-view resistance as a
  system-level assumption of every KT paper.
- **OPTIKS** (§ on usernames) describes the same identifier attack as KEYTRANS §9 (the
  server keeps `alice`, `alicefake`, `bob`, `bobfake`, and "no inconsistency will ever be
  caught") and adds a username → user id structure.
- **Consistency-or-Die** replaces auditors with VRF-selected committees of users, but
  states "we do not deal with Sybil attacks": unusable where the server controls
  registration, as every Nuschel server does.
- **PRISM** (a zkVM rollup on a data-availability layer) and **IronDict/Aegon**
  (polynomial commitments; Aegon reports constant audit proofs under 30 KB against AKD)
  aim at client self-auditing at billions of users. New cryptography and research code;
  Nuschel's scale does not need them.
- **Transparency-log witnessing.** C2SP specifies checkpoints, cosignatures, the witness
  protocol and quorum policies; the Witness Network has six staging witnesses
  (GDB Security, Geomys, IPng Networks, Markovian Protocol, Mullvad VPN, TrustFabric) and
  approves logs by hand. The Go checksum database relies on many proxies making a victim
  hard to identify ("the attacker would have to … arrange to identify the victim well
  enough to always serve the forked log to the victim").

---

## 3. What Nuschel does (from the code)

- **One akd log per server** (`AkdDirectory`, akd 0.13.0, BLAKE3, ECVRF-EDWARDS25519).
  The only label published is `acct:<account id>` (`apps/server/src/kt.rs`, `label()`).
  `name:` and `id:` exist as label types and `NameBinding` as a value format, but nothing
  publishes them. Value format 3: identity root public key (1986 bytes), roster
  commitment, hash of the identity's `AccountStatement`.
- **Heads.** `SignedHead { log_id, epoch, root, issued_at_ms, Ed25519 signature }`, 148
  bytes, signed by the server identity key; `log_id` hashes the 71-byte descriptor
  (signing key and VRF key). A new epoch at most every 10 s while bindings wait; while
  nothing changes, the current epoch is re-signed hourly with a newer `issued_at`, which
  is clamped never to decrease (`refresh_head`, `publish`).
- **Clients** (crypto worker): verify the head, require `issued_at` within 25 h (1 h of
  future tolerance), compare with up to 64 known heads (equivocation and stale-signing
  evidence), and audit akd's append-only proofs from the last accepted head (per-epoch
  proofs stored by the server and joined; at most 4096 epochs per request). The first
  head of a log is accepted without audit, and the log descriptor is pinned on first use
  per `host:port`.
- **Monitoring.** Own `acct:` history on connect and daily; histories of DM and verified
  contacts; lookups of other contacts when their profile or a DM opens. Every check
  downloads the complete history (`HistoryParams::Complete`).
- **Evidence and failures.** Two signed heads for one epoch with different roots
  (equivocation), or an older epoch signed after a newer one (stale signing), are kept as
  transferable evidence; failed audits between signed heads count as misbehaviour;
  "not found" is unauthenticated and treated as a failure.
- **Gossip.** `HeadGossip` and `check_gossip` exist and the worker answers `MakeGossip`
  and `CheckGossip`, but no MLS message carries them.
- **Auditors.** None. No countersignature field exists in heads, descriptors or the
  protocol.
- **UI.** A KT pass is shown as "Not verified" (no "verified automatically" state); a
  `not_found` lookup is shown as "No identity on this server. They have not used a
  Nuschel client with key transparency here yet." (`apps/desktop/src/identity.rs`).

---

## 4. Gap analysis

| Property | Signal | WhatsApp | Nuschel | Sev |
| --- | --- | --- | --- | --- |
| Split-view resistance | 3 auditors, all required, ≤ 7 days lag [code] | Cloudflare witness (sync) + monitor (async); client enforcement unverified | None required; first head TOFU | Critical |
| Gossip | None | None documented | Format only; in-band if shipped | High |
| Freshness bound | Server head ±24 h; tree size cannot be re-signed; auditor ≤ 7 days [code] | Auditor timestamps; client rule unverified | 25 h on a server-asserted, re-signable time | High |
| Identifier coverage | ACI, E.164, username hash [code] | Phone number → keys | Account id only; names and registration numbers outside KT | High |
| Federation | n/a (one operator) | n/a | One log per server, no cross-server witnessing | High |
| Log state on first use | Auditor-signed | Auditor-signed (if enforced) | Trusted on first use | Medium |
| History / monitoring size | Binary ladder, logarithmic per check (-06 §8) | akd, optimised for many versions (Messenger post) | Full history, ≈ 4.2–5.3 KB per version (§5.8) | Medium |
| Audit cost | Auditors | Auditors | Clients audit every skipped epoch (≈ 1–2 KB per changed account, §5.8) | Low |
| Absence proofs | Prefix-tree non-inclusion | Not in akd's API | Unauthenticated, handled as failure; UI wording | Low |
| Deletion / compaction | Maximum lifetime (-06 §7.1) | Keys removed on account deletion; Parakeet compaction | None; root key in akd values forever | Low |
| Censorship (owner's update withheld) | Owner detects | Parakeet §V-C option | Owner's client retries; no alarm found | Low |
| Post-quantum | Ed25519 | Ed25519 | Ed25519 heads; classical VRF | Low |

### 4.1 Split-view resistance without auditors (G1, Critical)

A malicious server can show Bob a log in which Alice's account is bound to its own
identity, and show Alice a log in which it is not. Both views verify; Alice's
self-monitoring sees her real binding; Bob's audits run within his fork. Nothing a
client checks comes from outside the server: no auditor, no witness, no anonymous
channel, and gossip is not transported (and would not help, §5.2). This is precisely the
configuration KEYTRANS calls "not secure" and that the SoK lists as the first
system-level assumption of every KT design. Signal closes it with mandatory auditor
signatures; Parakeet with a witness quorum.

What KT still gives today: an honest server's mistakes are caught; a malicious server
that does *not* fork leaves its changes visible to the owner; and the TOFU pin turns a
later rebinding into a visible key change. A targeted first contact, or a
targeted rebinding of an unverified contact, is not detected.

### 4.2 Gossip not transported (G2, High)

`MakeGossip`/`CheckGossip` exist; nothing carries them. §5.2 explains why carrying them
inside MLS would not close G1. Gossip still has small value (an honest server's
accidental fork, partial attacks), but it should not be presented as a defence.

### 4.3 Head freshness (G3, High)

`issued_at` is written by the server. By design the server re-signs its current epoch
with a new time, so a server that freezes Bob at epoch 5 while publishing epoch 6 to
everyone else can keep handing Bob fresh-looking heads for epoch 5. The only detection is
stale-signing evidence, which needs Bob to hold a head of epoch 6 signed earlier than his
newest head of epoch 5, which only gossip could deliver. A frozen view hides revocations
and rotations from Bob for as long as the server likes. In Signal a tree size cannot be
re-signed with a new timestamp [code], and the auditors' timestamps bound the lag. In a
witnessed design the witnesses' timestamps provide the bound (C2SP: a checkpoint
cosignature states "as of the specified time, this is the largest consistent tree the
cosigner has observed").

### 4.4 Identifiers outside KT (G4, High)

Users see Mumble names and registration numbers; "only the server maps [them] to an
account" (e2ee.md, Phase 3). The server can answer Bob's request for "alice" with an
account it controls, bound consistently in KT to its own identity. KT is consistent for
both accounts, Alice's self-monitoring sees nothing, and Bob's first contact is with the
impostor. The worker's memory of number → account only catches a later change. This is
KEYTRANS §9's and OPTIKS's example. Signal puts E.164 and the username hash in the log.
The fix is cheap with akd: publish `name:<name>` → account id and a registration-number
label → account id (secondary identities in KEYTRANS's terms), monitored by the owner
like `acct:`.

### 4.5 Federation and per-server logs (G5, High)

Each server's log speaks only for its own accounts, which is correct (KEYTRANS §5.3).
Cross-server identity records are signed by the identity, so a server can omit or delay
but not forge. What is missing is any party outside a server that holds the server to
one history. §5 covers this; the short version: a client's other servers are useful
extra witnesses but cannot be the witness set, because two clients with disjoint server
sets have disjoint witnesses, and then a fork needs no dishonest witness at all.

### 4.6 TOFU interplay (G6, Medium)

Three things are trusted on first use: the identity root (by design, §4 of e2ee.md), the
log descriptor per `host:port`, and the first head of a log ("Clients never audit from
epoch 0"). The third is the dangerous one with G1: a new client can be put on a private
fork from its first connection. With witnesses, the first head must carry a quorum too,
and log state is no longer trusted on first use. The log key remains TOFU, but the auditor's
enrolment check, which binds each origin to one log key as seen from the network
(§5.7), gives a second opinion on "one log per server".

### 4.7 History and monitoring proof size (G7, Medium)

Measured (§5.8): a history proof costs about 4.2 KB per version at 100 accounts and 5.3
KB at 10,000; 40 versions are 170–212 KB. The 2018-byte value is about 40–48 % of
that: the root public key is repeated in every version. Versions accumulate quickly:
every device added or removed, every yearly certificate renewal (the worker renews 30
days before expiry) and every revocation changes the roster commitment. A user with
three devices gains at least three versions a year; after ten years a full history is
around 200 KB, fetched on every connect and daily for every DM and verified contact
(50 contacts: about 10 MB a day). KEYTRANS monitoring is logarithmic per check (binary
ladder over the log tree). Options: incremental monitoring from a verified checkpoint
(akd 0.12 added `MostRecent` history proofs; their soundness with a stored checkpoint
needs review before use), commit to the root key's hash rather than the key, and a
longer interval for contacts than for one's own labels.

### 4.8 Audit cost over time (Low)

Per-epoch audit proofs, stored while the epoch is newest and joined later, are minimal
and correct (they link two fixed roots). Clients audit every skipped epoch: about 1–2 KB
per changed account in its own epoch, less when batched (§5.8). Affordable at 100 to
10,000 accounts; above that, auditor cosignatures should cover large gaps.

### 4.9 Absence proofs (G9, Low)

akd has no public non-membership API, and Nuschel correctly treats `not_found` as a
failure. The desktop, however, states it as fact ("They have not used a Nuschel client
with key transparency here yet"). It should say that the server says so. Ensure that no
path offers an unencrypted conversation because of a `not_found`.

### 4.10 Own refinements, checked for soundness (G8, Low)

- **Account statements** (`AccountStatement`: identity, signing generation, log id,
  account id, creation time; hybrid signature by the device-signing key). Sound for
  their purpose: a server cannot bind someone else's identity to one of its accounts,
  and "same identity as on X" cannot be faked, because the statement names the log id
  (which commits to both log keys) and the immutable account id. Replay on the same log
  and account only re-asserts a true claim; a republished *older* value shows up in
  self-monitoring as a version the devices did not make. They do not address G4: the
  server still chooses which account a name refers to.
- **Roster commitment.** SHA-256 over a context label, the device-signing certificate
  hash and two sorted, counted lists of 32-byte hashes (devices, revocations). The
  encoding is injective (fixed-size elements, u16 counts below the 4096 cap), duplicates
  are refused, and one certificate per device id is enforced after the commitment check.
  Sound. Its cost is G7: every roster change is a new version carrying the full root key.
- **Stale-signing evidence.** Sound given the clamp: an honest server's head for epoch e
  is always signed no later than its first head for e+1 (`publish`, `repair` and
  `open` all sign at `max(now, latest.issued_at)`; `head_at` returns stored heads and
  never signs). Two caveats. It is dormant until heads are exchanged (G2/G3). And the
  clamp turns one forward clock jump of more than an hour into a long outage: clients
  reject heads from the future, and the clamp keeps every later head in the future until
  real time catches up. With witnesses, re-witnessing replaces re-signing, and this
  evidence becomes a secondary check.
- **Strict Ed25519, `catch_unwind` around akd's verifiers, the membership-proof label
  fix, epoch caps.** Good hardening.

---

## 5. Witnessing and auditing for a federation of akd logs

### 5.1 Two jobs

A **witness** keeps the latest head it has endorsed for each log and endorses a new head
only if it extends that one; this gives uniqueness (no split view) and, with a
timestamp, freshness. An **auditor** checks that each transition is a valid update of
the directory (for akd: the append-only proof). Parakeet's witness does both in one
step; Cloudflare runs them as two services with two keys; C2SP witnesses do only the
first, for RFC 6962 Merkle logs.

### 5.2 Why gossip inside MLS fails exactly when it matters

The coordinator's claim holds, with one refinement.

- Consider a targeted MITM of Alice and Bob on server S. S shows Bob a fork where Alice
  is bound to S's identity, and runs two groups: Alice with a fake Bob, and Bob with a
  fake Alice. Every MLS message between Alice and Bob, including any `kt_head`, is
  decrypted and re-encrypted by S's fake devices, which can replace Alice's head with one
  from Bob's fork. Nothing contradicts.
- S cannot instead add one ghost device to a shared group: every member sees the roster
  and checks each credential against its own KT view, so other members (and Bob's own
  devices) would reject a ghost. The attack must replace the whole conversation, which
  is the case where in-band gossip is entirely under the attacker's control. This is
  the difference from iMessage (§2.3).
- The refinement: gossip does help against forks that are *not* paired with a full MITM
  of all the victim's conversations. If Bob is given a fork about some third party Z, his
  heads reach honest peers in his other conversations, and dropping the messages that
  carry them is visible in the per-sender hash chains (e2ee.md §6.4). But the attack KT
  is meant to stop, a server compelled to intercept one person, is exactly the targeted
  full MITM. KEYTRANS §3.3 requires gossip over an *out-of-band* channel that forms a
  connected graph; Parakeet §V rejects client gossip for eclipse attacks.

Conclusion: split-view resistance needs parties outside the operator's control that the
client checks before trusting a head. Gossip can stay as a cheap extra, not as the
defence.

### 5.3 Quorum intersection: the constraint that decides the design

An honest witness never endorses two inconsistent heads, but the log chooses which
lineage each honest witness sees. If Alice accepts heads endorsed by k₁ of a set and Bob
by k₂ of the same set of n witnesses, of which f may be dishonest, a split view is
impossible only if k₁ + k₂ > n + f (Möller, eq. (1)). Symmetric policies need
k > 2n/3 to tolerate f = n − k; with all witnesses honest, a majority suffices (KEYTRANS
architecture §4). Parakeet uses 2f+1 of 3f+1.

The consequence for federation: **clients must draw their quorums from a common set.**
Suppose Alice is on S and T and requires T's cosignature on S's log; Bob is on S and U
and requires U's. S shows T and Alice lineage A, and U and Bob lineage B. T and U are
both honest, each saw one consistent lineage, and the split view succeeds. This is
CONIKS Appendix B's partition attack with one auditor per client.

### 5.4 Who chooses the witnesses (Sybil resistance)

The witnessed server must never choose; and "the client chooses" must still yield
intersecting quorums. The user's decision (2026-11-11): **one project-run auditor,
required by default once it is deployed, configurable in the client's settings and the
Home Manager module, plus the other Nuschel servers a user is connected to as
client-picked witnesses.** Evaluated against §5.3:

- **The project auditor (default, required).** Every client that keeps the default
  requires the same auditor, so every pair of clients' quorums intersect in it: a split
  view needs the auditor's collusion or compromise. This is a 1-of-1 common set. It
  tolerates no dishonest member and no outage (an outage makes verification
  "unavailable", never silently weaker; §5.9). It is the minimum that covers the common
  case below, and the policy can grow to k of n by adding witnesses in a release.
- **The common case: Alice and Bob both only on S.** Only the default auditor protects
  them; their own sets of servers are empty. Without the auditor there is no protection,
  which is why it must be on by default.
- **The other servers the user is connected to.** Picked by the client, never named by
  S. Not a common set, so they cannot replace the auditor; they add protection where
  users share servers: two users of S who are both also on T both have S's log checked
  by T, so a fork between them is caught at T even if the auditor colludes with S. Often
  run by the same operator as S, so their weight is pairwise, not global. Default:
  checked when available, never required (§5.7, rule C6).
- **Witnesses pinned by an administrator** (settings, `managed.json`). Fine for an
  organisation whose users all share the same pinned set: intersection holds within it.
  Removing the default auditor is allowed (an organisation that runs its own), but then
  only the organisation's users share a quorum.
- **Witnesses listed by the server operator.** Never part of the client's quorum (a Sybil
  vector). A server may push to more witnesses than clients require; that is harmless.

### 5.5 Where the witness software runs

Recommendation: **a small separate binary, `nuschel-witness`, for the project auditor,
and a built-in witness role in `nuscheld` for the server-to-server case**, with the
verification logic in one library module (`nuschel-kt::witness`) used by both.

- The project auditor's key is long-lived and worth more than any one server's; it
  should not share a process with a Mumble/TLS voice server and its attack surface. A
  separate binary needs only HTTP, a small state database and its key, runs with
  `DynamicUser` and a strict systemd sandbox, and speaks the standard C2SP witness API,
  so generic tooling (and later other witnesses) interoperates.
- The server-to-server role needs no new listener: a server witnesses other logs for its
  own logged-in users over the Nuschel protocol (§5.7, client-mediated submission). This
  is CONIKS's "providers as auditors" (§4.1.4) with the client, not the witnessed
  server, deciding what to submit.

### 5.6 Two layers: a log of map roots for generic witnesses, akd audits for validity

Generic C2SP witnesses verify RFC 6962 consistency proofs, not akd proofs. So each
server keeps a small **epoch log**: an append-only RFC 6962 Merkle tree whose leaf `i`
is akd epoch `i`'s head. This is KEYTRANS's shape (a log tree over map roots) on top of
akd. Duties then split:

- **Witnesses** (server witnesses, or any C2SP witness) cosign epoch-log checkpoints:
  one sequence of akd roots per server, and fresh timestamps.
- **The project auditor** does that and also verifies every akd transition before it
  cosigns (Parakeet's combined witness and auditor), so its cosignature means "unique
  and valid".
- **Clients** verify akd's append-only proof from their last accepted root to the new
  one, as they already do.

Is the split sound? Claim: if every root a client relies on is a leaf of a checkpoint
its quorum cosigned, and the client verifies an append-only path between consecutive
roots it relies on, then it gets KT's guarantee for everything it looked up. Argument:
witnessing makes the sequence of roots unique, so the root Bob used for a lookup is the
one Alice's monitoring sees. A malicious server could publish, to everyone, a sequence
that inserts a fake binding at epoch e and removes it at e+1; that is not a split view,
so a pure witness cosigns it. But Bob looked up at e, so his next audit goes from root e
to a later root, which must contain the leaf; the removal fails his audit. akd's
append-only property is transitive, so the intermediate roots the server supplies do
not matter for Bob's own safety, only the witnessed endpoints. What clients alone do not
cover: a client that saw the fake binding and never came online again (KEYTRANS's
contact monitoring has the same gap, architecture §7.1). The project auditor's akd check
covers it. So: a witness quorum is mandatory for split-view resistance; client audits
for each client's own safety; the auditor's akd check for clients that go away or skip
audits.

### 5.7 Protocol sketch and client rules

#### Epoch log

Leaf `i` (RFC 6962 leaf hash = SHA-256(0x00 ‖ leaf)):

```text
struct {
  opaque context[18] = "nuschel-kt/v1 leaf";
  opaque log_id[32];
  uint64 epoch;              // equals the leaf index
  opaque akd_root[32];
  uint64 published_ms;       // when the epoch was first signed; never decreases
} EpochLeaf;                 // 98 bytes
```

Checkpoint (C2SP tlog-checkpoint): origin `<host>:<port>/nuschel-kt/v1` (lower-case
host as in bookmarks), size = newest epoch + 1, root hash, signed by the server identity
key as an Ed25519 log signature. Signed notes cannot contain NUL and `SignedHead`
signatures start with `nuschel-kt/v1 signed head\0`, so the two formats cannot collide
under one key.

#### Messages (Nuschel extension, capability `kt-witness/1`)

`kt-witness/1` requires `kt/1` and is sticky like every capability: a client that once
saw it from a server refuses a later connection without it (extension.md §1). The hello
gains `bytes kt_witness = 6`: the server's own witness key as a C2SP vkey (type 0x04),
pinned like `kt_log`, present when the server offers the witness role.

```proto
// Attached to replies that carry a head.
message KtWitnessed {
  bytes checkpoint = 1;          // signed note: log signature and cosignatures, ≤ 16 KiB
  repeated bytes inclusion = 2;  // RFC 6962 inclusion proof of the head's EpochLeaf, ≤ 63 × 32 bytes
}
// New field in KtStatusReply (4), KtLookupReply (5), KtHistoryReply (5), KtHeadReply (next free).

// 0x4E30 -> KtConsistencyReply: a consistency proof in this server's epoch log.
message KtConsistency { uint64 request_id = 1; uint64 old_size = 2; uint64 new_size = 3; }
// 0x4E31
message KtConsistencyReply { uint64 request_id = 1; repeated bytes proof = 2; }  // ≤ 63 hashes

// 0x4E32 -> KtWitnessStatusReply: the newest checkpoint this server cosigned for another log.
message KtWitnessStatus { uint64 request_id = 1; string origin = 2; bytes log_id = 3; }
// 0x4E33; checkpoint empty when the origin is unknown here.
message KtWitnessStatusReply { uint64 request_id = 1; bytes checkpoint = 2; }

// 0x4E34 -> KtWitnessSubmitReply: ask this server to witness another server's checkpoint.
message KtWitnessSubmit {
  uint64 request_id = 1;
  bytes descriptor = 2;          // the other log's LogDescriptor (≤ 512 bytes)
  uint64 old_size = 3;           // the size this server last cosigned for the origin, or 0
  repeated bytes proof = 4;      // consistency proof old_size -> new size, from the other server
  bytes checkpoint = 5;          // the other server's signed checkpoint
}
// 0x4E35
message KtWitnessSubmitReply {
  uint64 request_id = 1;
  oneof result {
    bytes cosignature = 2;       // C2SP cosignature line for the checkpoint
    bytes conflict = 3;          // old_size did not match: the newest cosigned checkpoint here
  }
}
```

A server witnesses for its own registered, connected users only; limits
`kt.witness_submits_per_minute` (default 30 per connection), `kt.witness_max_logs`
(10,000 logs), request size 8 KiB. It keeps one checkpoint per (origin, log id), so a
user who submits a made-up key for someone else's origin blocks nothing, and checks and
updates it atomically (tlog-witness's race). Malformed or wrongly signed input is `Error{invalid}`;
an inconsistent proof is `Error{invalid}` and is logged with both checkpoints, since it
may be evidence.

#### Project auditor (`nuschel-witness`, HTTP)

- `POST <prefix>/nuschel-kt/v1/add-epochs`: body in nuschel-kt's strict codec:
  descriptor, old size, the new `EpochLeaf`s, one akd append-only proof per new epoch,
  and the signed checkpoint at the new size. The auditor checks the log signature and
  the origin's enrolled key (below), that old size equals its stored size (else 409 with its
  size, as tlog-witness), each leaf (log id, consecutive epochs, non-decreasing
  `published_ms`), each akd proof from its stored newest root, appends the leaves to its
  compact range (O(log n) hashes; no consistency proof needed), requires the resulting
  root to equal the checkpoint's, persists, and answers with a timestamped Ed25519
  cosignature (`cosignature/v1`). Its cosigner name carries the statement "witnessed and
  akd-audited" (one name, one set of statements: tlog-cosignature).
- `POST <prefix>/add-checkpoint` (C2SP tlog-witness), accepted only for an unchanged
  size: the hourly refresh of the timestamp.
- `GET <monitoring prefix>/<origin hash>/checkpoint` (C2SP), for monitors.
- **Enrolment** of a new origin (`accept_new_logs`, at most `max_logs`): the auditor
  connects to `host:port` as a Nuschel client, reads `kt_log` from the server's hello
  and requires it to equal the submitted descriptor, the way ACME checks control of a
  name. A second log id for an enrolled origin is refused and logged (a server showing
  two logs is evidence; a key rotation is a new origin version, `nuschel-kt/v2`).
  Per-origin rate limit. Its first state for a log is trusted on first use (KEYTRANS's
  `auditor_start_pos`); clients audit across that point with akd as usual.

#### Server (`nuscheld`)

1. Append an `EpochLeaf` in the same storage batch as each akd epoch (the stored per-epoch
   audit proof is already written there).
2. For each configured witness (`[[kt.witnesses]]`, the project auditor by default once
   released): after new epochs, at most once a minute, `add-epochs` (auditors) or
   `add-checkpoint` (plain C2SP witnesses); hourly, resubmit the unchanged checkpoint
   to refresh timestamps; answer 409 by resending from the witness's size. Keep the
   newest checkpoint with every cosignature received; serve it in `KtWitnessed`.
3. Hourly re-signing of akd heads stays for old clients; freshness now comes from
   cosignatures.

#### Client: the witness set

- **Built-in** (`nuschel_kt::DEFAULT_WITNESSES`, one constant used by the client and by
  nuscheld's defaults): name, vkey, submission URL, `required`. **Empty until the
  auditor is deployed.** Deploying = putting its vkey and URL in the constant and
  releasing; no user action.
- **Settings** (`key_transparency` in settings.json, overridable key by key from
  managed.json): see the configuration below. The effective set W is the built-in
  witnesses unless `default_witnesses` is false, plus the configured ones that are
  enabled. R ⊆ W: those marked required (the built-in auditor is required).
- **Server witnesses**: every other server the client is connected to that offers
  `kt-witness/1`, with its pinned `kt_witness` key; picked by the client, governed by
  `server_witnesses`.

#### Client rules

- **C0. While R is empty** (today, and until the auditor ships): behave exactly as now.
  No cosignature is required and no status changes. If a server offers `kt-witness/1`,
  still check the log signature on its checkpoints and the inclusion proofs (a failure
  is a KT failure as today), and run server witnesses (C6) in check mode, so the code
  path is exercised before it matters. Never show anything stronger than today.
- **C1. Pins.** Per server: the log descriptor (as now), the origin (must equal the
  bookmark's `host:port`), and the server's own `kt_witness` key if offered.
- **C2. Accepting a head when R is not empty.** Accept an akd head only if (a) it is
  server-signed; (b) its `EpochLeaf` is included at index `epoch` of a checkpoint signed
  by the log; (c) every witness in R has cosigned that checkpoint with a timestamp no
  older than `witness_max_age_hours` and at most one hour in the future; (d) a head
  taken as the newest (status, lookup, history) is the checkpoint's last leaf, while
  older heads (audit steps, `KtHead`) only need (b); (e) it is consistent with known
  heads and checkpoints; (f) akd's append-only proof from the last accepted head
  verifies.
- **C3. First head.** C2 applies to the first head of a log too: no log state on first
  use once R is not empty.
- **C4. Pending.** A head newer than the newest cosigned checkpoint (published within
  the last minutes) is *pending*: usable like an unverified contact, re-checked within
  ten minutes; still uncovered after that counts as a failure.
- **C5. Failures.** No `kt-witness/1` on a server while R is not empty, or a required
  cosignature missing, or older than the bound: "Automatic verification unavailable"
  for that server (a failure, retried; e2ee.md §5.5), never misbehaviour. Messages keep
  using pinned keys; never block messaging on a witness outage alone. The first time a
  server is seen after the auditor ships, without `kt-witness/1`, the status says that
  this server's key transparency is not witnessed.
- **C6. Server witnesses** (`server_witnesses`: `off`, `check` (default), `require`).
  For server S and each other connected server T with `kt-witness/1`, at most hourly:
  `KtWitnessStatus{S's origin and pinned log id}` to T; if T has none or an older one, `KtConsistency`
  from S and `KtWitnessSubmit` to T. A cosignature is kept with S's checkpoint. A
  `conflict` is followed by `KtConsistency` from S between T's checkpoint and the
  client's; if S cannot prove consistency, or the two checkpoints have the same size and
  different roots, that is misbehaviour with both S-signed checkpoints as evidence.
  `require`: at least one server witness's fresh cosignature is needed as well,
  otherwise C5.
- **C7. Evidence.** Two log-signed checkpoints that cannot both be honest, or two signed
  akd heads for one epoch with different roots: keep, show, offer to send to the
  project.
- **C8. Privacy.** Clients never contact the project auditor or other witnesses over
  HTTP; the server relays cosignatures, and server witnesses are reached over existing
  connections.

#### Configuration

Client, `settings.json` / `managed.json` (`ClientSettings::key_transparency`, type in
`nuschel_ipc::crypto` like `RecoveryPolicy`, passed to the crypto worker):

```json
"key_transparency": {
  "default_witnesses": true,
  "witnesses": [
    {
      "name": "kt.example.org/witness",
      "key": "kt.example.org/witness+1a2b3c4d+BHHk…",
      "enabled": true,
      "required": true
    }
  ],
  "server_witnesses": "check",
  "witness_max_age_hours": 24
}
```

Checks: at most 16 witnesses (signed-note verifiers must accept 16 signatures); names
as C2SP cosigner names (≤ 255 bytes, no spaces, `+` or control characters); vkey types
0x04 (Ed25519 cosignature) or 0x06 (ML-DSA-44); 1 ≤ `witness_max_age_hours` ≤ 168. A
managed `key_transparency.witnesses` replaces the user's list and is shown locked; a
system-wide `/etc/xdg/nuschel/managed.json` works as for other settings. The desktop
gets a settings page: the list with its source (built-in, managed, own), enable
switches, add by pasting a vkey, and the server-witness mode.

Home Manager (`programs.nuschel`): add `key_transparency` to `settingNames` in
`nix/home-manager.nix` (and its test), so
`programs.nuschel.settings.key_transparency = { … }` reaches managed.json; optionally
typed options `programs.nuschel.keyTransparency.{defaultWitnesses, witnesses,
serverWitnesses, witnessMaxAgeHours}` that render into it.

Server, `nuscheld.toml`:

```toml
[kt]
default_witnesses = true          # push to nuschel_kt::DEFAULT_WITNESSES
witness_role = true               # answer KtWitness* for this server's users
# witness_key_file = "/run/credentials/nuscheld.service/kt-witness-key"
witness_submits_per_minute = 30
witness_max_logs = 10000

[[kt.witnesses]]                  # additional witnesses to push to
name = "kt.example.org/witness"
key = "kt.example.org/witness+1a2b3c4d+BHHk…"
url = "https://kt.example.org/witness"
audit = true                      # Nuschel auditor (add-epochs) rather than plain C2SP
```

The witness key is separate from the server identity key (an Ed25519 cosigner key must
not be shared across cosigner roles); generated once in `state_directory`
(`kt-witness.key`) if no file is given.

NixOS, `services.nuschel` (nix/module.nix): `kt.defaultWitnesses` (bool, true),
`kt.witnesses` (list of `{ name, key, url, audit }`), `kt.witnessRole` (bool, true),
`kt.witnessKeyFile` (LoadCredential, asserted not in the Nix store like the other keys).

NixOS, new `services.nuschel-witness`: `enable`, `package`, `name` (cosigner name),
`keyFile` (LoadCredential; required; not in the Nix store), `listenAddress` (default
`127.0.0.1`), `port`, `acceptNewLogs` (true), `maxLogs` (10,000),
`submissionsPerMinute` (per origin), `openFirewall`; `StateDirectory=nuschel-witness`,
`DynamicUser`, the same systemd hardening as `services.nuschel`; TLS through the
operator's reverse proxy (C2SP allows a bastion); outbound network access for enrolment
checks. The service logs its vkey at start (and `nuschel-witness vkey --key-file …`
prints it) for `DEFAULT_WITNESSES`.

### 5.8 Costs (measured)

Release build, in-memory store, values 2018 bytes; sizes of the encoded responses in
bytes, as clients receive them.

| | 100 accounts | 1,000 | 10,000 |
| --- | --- | --- | --- |
| Audit, 1 changed account, one epoch | 940 | 1,534 | 1,798 |
| Audit, 1 new account, one epoch | 610 | 808 | 1,072 |
| Audit, 10 changes in one epoch | 3,976 | 8,068 | 12,160 |
| Audit, 10 changes over 10 epochs | 10,282 | 13,186 | 17,542 |
| Audit, 100 updates in one epoch (50 new, 50 changed) | 17,308 | 34,996 | 65,092 |
| Audit of the initial import | 6,682 | 66,082 | 660,082 |
| Lookup response | 5,010 | 6,121 | 6,828 |
| History, 3 / 10 / 20 / 40 versions | 13,796 / 44,167 / 87,203 / 170,458 | 14,806 / 47,500 / 94,071 / 184,699 | 17,331 / 54,873 / 108,211 / 211,565 |

Estimates from these (not measured): a client that is online daily on a 1,000-account
server with 20 binding changes a day audits about 20–30 KB a day; at 10,000 accounts and
200 changes, about 0.2–0.35 MB; batching changes into fewer epochs lowers both. The
project auditor receives the same per-epoch proofs once per server and verifies each in
about a millisecond per changed account (e2ee.md §5.8).

Witnessing adds per head delivery an inclusion proof of ⌈log₂ epochs⌉ hashes (a year of
epochs every 10 s is 22 hashes, 704 bytes; a quiet server far fewer) and the
cosignatures: an Ed25519 cosignature is 76 bytes raw (key id, timestamp, signature), an
ML-DSA-44 one 2,432 bytes. Witness load: at most 1,440 submissions a day per log at one a
minute, 24 for a quiet log; a witness stores one checkpoint (or, for the auditor, a
compact range of about 22 hashes and the newest akd root) per log.

### 5.9 Liveness and freshness

- **Bound.** `witness_max_age_hours`, default 24: libsignal accepts server heads up to
  24 h old and auditor heads up to 7 days; with hourly re-cosigning, 24 h is achievable
  and bounds a frozen view to a day.
- **The auditor offline.** Servers cannot refresh cosignatures; after the bound, clients
  show "Automatic verification unavailable" for every server (C5). Messaging is
  unaffected. With one required auditor there is no spare; Signal's 3-of-3 has none
  either (the RWC 2025 slides describe WhatsApp incidents where epochs stopped
  progressing and proofs grew 6x). A second deployed witness, as a k-of-n group, is the
  fix when it matters.
- **A server that cannot reach the auditor** (or will not push) shows the same state to
  its own clients only: visible, not silent.
- **New bindings** become verifiable after the next cosigned checkpoint (about a minute
  plus the auditor's latency), comparable to e2ee.md §5.4's "pending" devices.

### 5.10 Privacy

- The project auditor learns each server's host name and port, how many epochs it has,
  when, and how many leaves each epoch changes; nothing about labels or values. KEYTRANS
  §8.2's fake updates can hide rates if that matters.
- If clients fetched cosignatures from witnesses, witnesses would learn which servers a
  user is on; so the server relays cosignatures (C8).
- **Server witnesses leak one thing**: a `KtWitnessSubmit` or `KtWitnessStatus` for S
  tells T that one of T's users is also on S, which T did not know (VRF labels keep it
  from finding out otherwise). With few users per server this links accounts across
  servers. It is the price of the feature the user asked for; `server_witnesses = "off"`
  removes it.
- Servers witnessing each other's logs learn each other's activity rate.

### 5.11 A cheap complement: comparison through another server

KEYTRANS lists anonymous communication as the third way to detect forks (§3.3), and
allows a federation member to proxy its users' queries to other logs (§5.3); the Go
checksum database relies on the same effect through proxies. Rule C6's `KtWitnessStatus`
is a comparison of this kind: T answers with what S showed T's other users. S cannot
tell which of T's users caused a submission. It fails if S and T collude.

### 5.12 Decisions

Made by the user (2026-11-11): akd stays; one project auditor, required by default once
deployed, configurable in settings and the Home Manager module; other connected servers
as client-picked witnesses; deployable on NixOS when ready.

Still open (questions in §8): the freshness bound; whether failures ever block first
contact or new devices; names under KT; the default for `server_witnesses` given its
privacy cost; and the cosignature algorithm (Ed25519, which C2SP witnesses accept today,
or ML-DSA-44, which C2SP recommends for new deployments; Ed25519 matches the rest of KT
today).

---

## 6. Options

### (a) Keep akd and add mandatory witnessing and auditing (recommended)

- **Effort.** `nuschel-kt`: epoch log (RFC 6962 inclusion and consistency, compact
  range), signed notes, cosignatures, policy evaluation, witness state machine.
  `nuscheld`: storage of leaves and checkpoints, a push client (needs an outbound HTTPS
  client with rustls: a cargo-deny review), the `KtWitness*` handlers, configuration.
  `nuschel-witness`: an HTTP service around the shared state machine and akd's audit
  verification. Crypto worker: rules C0–C8. Desktop: one settings page. Nix: two module
  changes. Plexi (Apache-2.0) is a usable reference for an akd auditor's message format.
- **Risk of implementing ourselves.** Low: Merkle inclusion and consistency proofs and
  signed notes are small, well specified and have public test vectors; akd's audit
  verification exists. The protocol risk is in the policy (quorum intersection, §5.3),
  which is a configuration decision, not new cryptography.
- **Interoperability.** C2SP checkpoints and cosignatures are what the Witness Network,
  Sigsum and the transparency-dev witnesses speak, so an independent operator can later
  witness Nuschel logs with off-the-shelf software and be added to the default policy.

### (b) KEYTRANS (off the table; comparison only)

What it would have given: a log tree native to witnessing, logarithmic monitoring
(binary ladder), distinguished entries, maximum lifetime and pruning, and one design
shared with Signal. What it would cost: no maintained permissively licensed
implementation (libsignal's `keytrans` and Trail of Bits' `signal-auditor` are
AGPL-3.0-only; Signal's server is AGPL-3.0); implementing it ourselves from a draft with
only partial formal analysis. On licences: EUPL-1.2 Article 5 lets a combined work be
distributed under a "Compatible Licence", and the Appendix lists AGPL v3. That permits
EUPL code to go into an AGPL work, not AGPL code into an EUPL work: reusing libsignal's
code would make the combined client AGPL. Option (a)'s epoch log captures the main
structural benefit (a witnessable log of roots) without switching.

### (c) Other designs from the literature

- **Parakeet witnesses** (akd-aware witnesses with a 2f+1 of 3f+1 quorum): the project
  auditor is one of these; growing to a quorum is the upgrade path.
- **CONIKS-style random auditor sampling** among many servers: detection probability
  depends on a large pool and random choice; Nuschel users have few servers.
- **Consistency-or-Die**: no Sybil resistance; unusable.
- **PRISM, IronDict, Aegon**: client self-auditing for billions of users through
  zero-knowledge proofs or polynomial commitments; research code, new assumptions, and
  PRISM depends on an external data-availability layer. Not justified at Nuschel's scale.

---

## 7. Recommendation and work plan

Keep akd. Make split-view resistance a requirement: the project auditor as the default
required witness once deployed, an epoch log so witnesses can serve, witness timestamps
as the freshness bound, server witnesses as client-picked additions. Keep client-side
akd audits. Bring names and registration numbers under KT. Keep "verified automatically"
out of the UI until heads carry the auditor's cosignature.

The steps below can be handed to an implementation agent in this order; each lists its
tests. Everything ships with `DEFAULT_WITNESSES` empty, so behaviour only changes when
the user deploys the auditor and releases its key (C0).

1. **Epoch log and checkpoints** (`crates/kt`, new modules `tlog.rs`, `note.rs`,
   `checkpoint.rs`): `EpochLeaf` encode/decode; RFC 6962 leaf and node hashes, inclusion
   and consistency proofs (generate and verify), compact range; C2SP signed notes
   (canonical base64, ≤ 16 signatures, unknown keys ignored, a known key that fails
   rejects the note), Ed25519 log signatures, `cosignature/v1` cosignatures, vkeys.
   Tests: RFC 6962/9162 and C2SP vectors (signed-note's example vkey and note); property
   tests (random trees: every inclusion and consistency proof verifies, every mutation
   fails); known-answer vectors in `tests/vectors/kt-v1.txt`; fuzz targets for note and
   proof decoders (round trip, no panics).
2. **Witness state machine** (`crates/kt/src/witness.rs`): state per (origin, log id)
   (descriptor, size, root, compact range, newest akd root), with an optional
   enrolled-key rule per origin for the auditor; `submit_checkpoint` (C2SP rules:
   404/403/400/409/422 equivalents), `add_epochs` (auditor: leaves and akd proofs),
   `refresh`; atomic check-and-update through the store trait. The network enrolment
   check lives in `nuschel-witness` (step 5) and hands the state machine an enrolled
   descriptor. Tests: each error case, the tlog-witness rollback race, a made-up key for
   someone else's origin blocks nothing (server-witness mode), a second log id for an
   enrolled origin refused (auditor mode), an inconsistent checkpoint kept as evidence,
   akd proof failure refused.
3. **Client policy** (`crates/kt/src/policy.rs`): `WitnessPolicy` (built-in + configured
   + server witnesses, required set, max age) and `check_witnessed(head, KtWitnessed,
   policy, now)` implementing C2–C4 and C7, returning accepted, pending or a `KtError`
   with its class. `DEFAULT_WITNESSES: &[WitnessConfig] = &[]`. Tests: R empty accepts
   as today (C0); R non-empty: missing, stale, future, wrong-key and wrong-checkpoint
   cosignatures fail; head not the last leaf fails; first head needs a cosignature;
   pending then covered; pending then uncovered fails.
4. **Server** (`apps/server`): leaves written with each epoch (`kt_records`); push task
   (`[kt] default_witnesses`, `[[kt.witnesses]]`, batching ≤ 1/min, hourly refresh, 409
   handling, metrics); `KtWitnessed` in `KtStatusReply`, `KtLookupReply`,
   `KtHistoryReply`, `KtHeadReply`; messages 0x4E30–0x4E35 with authorisation (registered
   and connected users), limits and errors; `kt-witness/1` and `Hello.kt_witness`;
   `witness_key_file`. Tests in `apps/server/tests/extension.rs`: negotiation and
   stickiness, replies carry valid checkpoints and inclusion proofs, rate limits, state
   per (origin, log id), conflict replies, a fake witness endpoint receiving
   `add-epochs`.
5. **`nuschel-witness`** (`apps/witness`): HTTP server (C2SP `add-checkpoint`,
   `nuschel-kt/v1/add-epochs`, monitoring `GET`), SQLite state, key from a credential
   file, limits, the enrolment check (a Nuschel handshake to `host:port` that reads
   `Hello.kt_log`, using `nuschel-protocol`); `vkey` subcommand. Tests: end to end with an
   in-process `nuscheld` pushing; enrolment against a fake server whose hello names
   another descriptor fails; restart keeps state; malformed and oversized bodies.
6. **Crypto worker and session** (`apps/crypto-worker`, `crates/ipc`,
   `crates/session`): the policy from settings in a `CryptoRequest` (as
   `RecoveryPolicy` is); C1 pins; C2–C5 on every head path (status, lookup, history,
   audit target); C6 driver over the session's connections; evidence storage. Tests in
   `apps/crypto-worker/tests/kt.rs` and `malicious.rs`: a server showing two clients two
   forks is caught by the required auditor (second submission refused) and by a shared
   server witness; disjoint server witnesses alone do not catch it (documents §5.3);
   freeze attack (re-signed old epoch) rejected once cosignatures are older than the
   bound; auditor unreachable gives "unavailable", not misbehaviour.
7. **Settings and desktop** (`crates/session/src/config.rs`, `apps/desktop`):
   `ClientSettings::key_transparency` with the checks in §5.7; managed keys locked; the
   settings page. Tests: config parsing and managed merging (managed list replaces,
   shown locked); invalid entries rejected like other settings.
8. **Home Manager** (`nix/home-manager.nix`, `nix/home-manager-test.nix`):
   `key_transparency` in `settingNames`; optional typed `programs.nuschel.keyTransparency`
   options. Test: the generated managed.json carries it and the client accepts it.
9. **NixOS** (`nix/module.nix`, new `nix/witness-module.nix` exported as
   `nixosModules.witness` beside `nixosModules.default`, `nix/vm-server.nix`):
   `services.nuschel.kt.{defaultWitnesses, witnesses, witnessRole, witnessKeyFile}`;
   `services.nuschel-witness`; a package output for `nuschel-witness`. VM test: a witness and a server on one machine, the
   server pushes, a CLI client requires the witness and accepts heads; stopping the
   witness yields "unavailable" after the bound (shortened in the test).
10. **Labels**: publish `name:` and a registration-number label (value: account id);
    self-monitor them; look up contacts by name or number through KT. Tests: a server
    mapping a name to another account is caught by the owner's monitoring.
11. **History cost**: measure incremental monitoring with akd's `MostRecent` against a
    stored checkpoint and review its soundness; consider committing to the root key's
    hash; separate intervals for own labels and contacts.
12. **Smaller fixes**: desktop wording for `not_found` ("The server says this account
    has no identity here"), and no unencrypted fallback on it; reconsider the
    `issued_at` clamp after a forward clock jump.
13. **Docs**: e2ee.md §5 (the corrections in §9; replace "optional auditors" and
    "gossip in MLS" with this design), extension.md (`kt-witness/1`, messages, limits),
    server.md (`[kt]` witness keys), a new docs section for operating `nuschel-witness`.
14. **When the user deploys the auditor**: run `services.nuschel-witness`, put its vkey
    and URL into `DEFAULT_WITNESSES`, release server and client together (servers first
    if possible, C5 covers the gap).

---

## 8. Questions for the user

1. **Freshness bound:** 24 hours (a frozen view lasts at most a day; the auditor must be
   reliable) or 7 days (Signal's auditor bound)?
2. **Failure policy:** when the required cosignature is missing or too old, only show
   "Automatic verification unavailable" (Signal's behaviour, and the proposal), or also
   block first contact and new devices?
3. **Server witnesses by default:** keep `check` as the default although it tells each
   of a user's servers that the user is also on the others, or default to `off`?
4. **Names under KT:** publish user names and registration numbers in the log, so that
   "alice" on a server is covered, accepting that renames become visible log entries?
5. **Cosignature algorithm:** Ed25519 now (C2SP witnesses and the rest of KT today) with
   ML-DSA-44 later, or ML-DSA-44 from the start for the project auditor?

### Answers (user, 2026-11-11)

- **Witness model: option B only.** A small fixed set of witnesses (the project
  auditor, perhaps a few more) that every client checks; **each server collects its
  own cosignatures** by submitting its checkpoints to the witnesses, so clients never
  relay heads between their servers and no server learns which other servers a user is
  on. The user: "lets just build B - it's the simplest and the least can go wrong with
  it." Not built: clients relaying heads between their own servers (option A), and
  arbitrary operators as policy witnesses (option C). Question 3 is therefore moot.
- **Witnesses are easy to set up on NixOS** (a module with sensible defaults).
- **Names under KT: yes**, user names and registration numbers are published in the
  log; renames become visible entries. Contacts stay keyed by identity (and local
  nicknames by identity) after first contact.
- **Cosignatures: ML-DSA-44 from the start** where possible.
- Still open: question 1 (proposed: 24 hours) and question 2 (proposed: show
  "Automatic verification unavailable"; block only adding new devices to an existing
  conversation; first contact proceeds as "not verified").

## 9. Proposed corrections to docs/e2ee.md

Not applied here.

1. **§5.1**, "runs in third-party-auditing mode with Cloudflare and Trail of Bits as
   auditors": Signal's production configuration has **three** auditor keys and clients
   require a head from **each**; Trail of Bits names the operators as Signal, Cloudflare
   and Trail of Bits. Signal's blog names only the two external ones. Add: auditor heads
   may be up to 7 days old; the server head must be within ±24 h; a tree size cannot be
   re-signed with another timestamp (libsignal `verify.rs`, `env.rs`).
2. **§5.1**, "based on an earlier IETF KEYTRANS draft": correct; add that Signal's signed
   configuration lacks protocol -06's `max_ahead`, `max_behind` and RMW fields, and that
   Signal uses a periodically updated `distinguished` label instead of -06's RMW
   algorithm.
3. **§5.1**, the label list (`acct:`, `id:`, `name:`): only `acct:` is published;
   names and registration numbers are mapped to accounts by the server outside KT
   (KEYTRANS architecture §9).
4. **§5.1**, "hourly otherwise, so that a fresh signed head always exists": the freshness
   is asserted by the server, which may re-sign an old epoch; it bounds an honest
   server's staleness, not a malicious server's.
5. **§5.3**, "Apple's Contact Key Verification gossips heads inside messages in the same
   way": Apple gossips log hashes "in the encrypted part of a small percentage of
   messages", but also verifies log consistency on the device and runs an internal
   auditor; and iMessage's per-device encryption keeps real devices in a conversation,
   unlike an MLS full MITM (this review §2.3, §5.2).
6. **§5.3**, "Without auditors, gossip and monitoring are the defence": gossip over the
   operator's own channel is not the out-of-band channel KEYTRANS §3.3 requires, and is
   not transported; without auditors or witnesses there is no split-view defence.
7. **§5.4**, "append-only since the last head seen (audit proof, or an auditor's
   countersignature)": no countersignature exists in any format; and the first head is
   accepted without audit.
8. **§4**, "Automatic verification shows 'verified automatically' when KT passes, as
   Signal does": Signal shows its green check only on the user's request and only with
   all auditor signatures, and only for contacts whose phone number is known. Nuschel's
   desktop shows no such state, which is right until heads carry a witness quorum.
9. **§5.2 table**, "No crate on crates.io" for KEYTRANS: still true for the protocol, but
   Trail of Bits' `signal-auditor` (a Rust auditor for Signal's variant, AGPL-3.0-only)
   is on crates.io, and Cloudflare's `plexi` (Apache-2.0) verifies akd audit
   signatures.
10. **§2, Threat model**, "show different people different keys without detection":
    not met today (G1); state it as the goal of the witness work.
