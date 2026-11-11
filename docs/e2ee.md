# End-to-end encryption, identity and key transparency

Status: design for review (2026-11-11). Phase 3 (identity and key transparency, without
MLS) is implemented: `nuschel-identity`, `nuschel-kt`, the crypto worker
(`apps/crypto-worker`), the `kt/1` directory in `nuscheld` and the client flow in
`nuschel-session` and the desktop; "Phase 3 as implemented" at the end lists what the
implementation settled. Phase 4 (MLS) is design only. It covers roadmap phases 3
(identity, key transparency) and 4 (MLS) in TODO.md, "Nuschel server", and builds on
the framing, identifiers and policies of [docs/extension.md](extension.md). Claims
marked *(spike)* are checked by tests in [`spikes/mls`](../spikes/mls), a standalone
crate outside the default build:

```sh
cd spikes/mls && CARGO_TARGET_DIR=... cargo test -j 4
cargo test --release --test scale -- --ignored --nocapture --test-threads 1
```

## Decisions at a glance

| Question | Decision |
| --- | --- |
| MLS library | OpenMLS with the libcrux provider. Prototype on 0.9.2; ship persistent groups only on the next release (§1.1) |
| Ciphersuite | `MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519` (X-Wing, draft-07 code point 0x0010); later the draft's X-Wing + ML-DSA-44 suite |
| Identity | One identity per person, independent of servers: a hybrid identity root key certifies a device-signing key, which certifies per-device keys (§3) |
| Signatures | Identity root and device-signing keys: hybrid Ed25519 + ML-DSA-65 from day one. MLS leaf keys: Ed25519 until OpenMLS releases ML-DSA suites |
| Encryption keys | X-Wing everywhere: MLS HPKE, device linking, key delivery outside MLS |
| MLS credential | A compact device certificate (~170 bytes); the per-person part comes from the server's directory and is covered by key transparency |
| Root key custody | Encrypted under a recovery key; linked devices hold the device-signing key, not the root |
| Device linking | Short code with SPAKE2 (magic-wormhole style) or QR, then an X-Wing-sealed provisioning bundle |
| Trust | TOFU on the identity root, safety numbers over both roots, verification valid on every server |
| Key transparency | One log per Nuschel server; `akd` behind a narrow interface; self-monitoring, contact monitoring, gossip of signed heads in MLS, optional auditors |
| Groups | One MLS group per conversation, bound to server and `ConversationId`; channel membership follows `can_read`, proposed by the server, committed by members |
| Delivery service | In `nuscheld`; handshake messages as PublicMessage validated with OpenMLS `PublicGroup`; one commit per epoch; shared `seq` stream |
| History for new members | Per-conversation history keys, rotated on removal and monthly, granted to new members inside MLS (§6.6) |
| Voice | SFrame (RFC 9605) with the MLS exporter; on the Mumble UDP transport too |
| Ordering | Per-sender counter and previous-message hash inside every payload, plus the last message the sender saw (§6.4) |
| Downgrades | Sticky server policy, per-conversation pins in the client, required capabilities in the group context |

Open questions for the user are collected in §10.

## 1. The stack: OpenMLS with libcrux (spike results)

### 1.1 Versions and the ciphersuite

- [openmls 0.9.2](https://crates.io/crates/openmls/0.9.2) (2026-10-22),
  [openmls_libcrux_crypto 0.4.0](https://crates.io/crates/openmls_libcrux_crypto/0.4.0),
  openmls_traits 0.6.0, openmls_memory_storage 0.6.0, all MIT, MSRV 1.91. They build
  with this workspace's toolchain (Rust 1.99.1, edition 2024) *(spike)*.
- With the feature `draft-ietf-mls-pq-ciphersuites`, the libcrux provider lists
  0x0001, 0x0003 and **0x004D `MLS_256_XWING_CHACHA20POLY1305_SHA256_Ed25519`**: X-Wing
  (ML-KEM-768 + X25519, X-Wing draft-06), ChaCha20-Poly1305, SHA-256, Ed25519. Init and
  leaf keys are 1216 bytes, encapsulations 1120 bytes *(spike: `suite.rs`)*.
- It is **experimental**: behind a draft feature, with OpenMLS-specific code points.
  [draft-ietf-mls-pq-ciphersuites-07](https://datatracker.ietf.org/doc/draft-ietf-mls-pq-ciphersuites/)
  (2026-11-01, submitted to the IESG) requests different values: 0x0010
  `MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519` with HPKE KEM 0x647a
  ("MLKEM768-X25519", identical to X-Wing per
  draft-irtf-cfrg-concrete-hybrid-kems), 0x0011 (AES-256/SHA-384), and 0x0020
  `MLS_128_MLKEM768X25519_CHACHA20POLY1305_SHA384_MLDSA44`. 0x004D is not in the draft.
- `supports()` in the libcrux provider checks only AEAD, hash and signature, so
  OpenMLS's provisional 0x004F (`MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519`) and
  0x004E also work end to end; the pure ML-KEM suite 0xF042 fails at group creation
  *(spike)*.
- OpenMLS `main` (unreleased) already moves on: the HPKE KEM id of X-Wing becomes
  0x647a, X-Wing key derivation follows draft-ietf-hpke-pq (`legacy-pq-key-derivation`
  exists only to keep 0.4.0 groups on 0x004D readable), libcrux crates are bumped (fixing
  the advisories in §1.4), and the libcrux provider gains ML-DSA (#2170) and more hybrid
  KEM suites (#2281).

**Decision.** Use OpenMLS + libcrux. Build the crypto worker and delivery service on
0.9.2, but create persistent groups only once a release carries the draft's code points
and the fixed libcrux crates. Target `MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519`:
it is the draft's X-Wing suite (libcrux's portable AES is bitsliced, without lookup
tables; x86-64 and NEON use the AES instructions), and its successor with ML-DSA-44
keeps the same KEM. The suite id lives in every key package and group
context, so a code point change later means a group migration (§1.5). Prototyping with
0x004D is fine; nothing persistent may depend on it.

### 1.2 What the spike demonstrates

| Test | Result |
| --- | --- |
| `group::three_party_lifecycle` | Create, add two by key package (one Welcome), verify credentials and binding before joining, messages with the sender's verified identity, removal; the removed member is inactive, cannot export or decrypt; the exporter changes per epoch and agrees across members |
| (same) | A replayed message fails (`SecretReuseError`): message keys are deleted after use |
| `group::post_compromise_security_needs_the_victims_own_update` | A copy of Bob's state follows Alice's update commit and reads the next message; after Bob's *own* update it is locked out (`OwnCommitMismatch`, `WrongEpoch`) |
| `group::forward_secrecy_across_epochs` | With `max_past_epochs = 0` (default) a late message from the previous epoch is lost (`TooDistantInThePast`); with 1 it is readable |
| `delivery::server_sees_metadata_not_content` | A 27-byte message is a 176-byte PrivateMessage; epoch, group, size and roster are visible; commits are PublicMessage |
| `delivery::concurrent_commits_one_wins` | Two commits in one epoch: the server accepts the first, rejects the second (`StaleEpoch`); the loser catches up and retries; all agree on the epoch authenticator |
| `delivery::server_rejects_tampered_and_stale_messages` | The server rejects a bad signature, but cannot check membership or confirmation tags (MACs under epoch secrets): members reject those (`InvalidMembershipTag`). Application messages for an old epoch are refused and resent |
| `delivery::external_join_reads_from_its_join_on` | External join from the server's GroupInfo and tree; the joiner cannot read earlier messages |
| `delivery::server_cannot_add_members_silently` | A forged commit fails (`InvalidSignature`) at the server and at members; an external join with the server's own identity shows a stranger; a copied credential fails verification |
| `delivery::pinned_conversation_cannot_be_replaced_or_downgraded` | Plaintext for a pinned conversation, and a Welcome into a fresh group with the same binding, are refused; an announced replacement generation is accepted |
| `storage::*` | OpenMLS state in an AEAD-encrypted, batched store survives a restart; tampering is detected; re-keying works; what is persisted (§1.3) |
| `identity::*` | Hybrid certificate chains; stripping the ML-DSA half fails; two devices of one identity; device linking over X-Wing HPKE; safety numbers; key rotation without identity change; the Mumble binding's limits |
| `scale` (release) | Costs at 10 to 1000 members (§6.7) |

### 1.3 Storage

OpenMLS persists everything through `StorageProvider` (openmls_traits 0.6, ~60
methods). Each reduces to put, get or delete of a serde value under
`(label, key, version)`. Labels written in the spike: `Tree`, `GroupContext`,
`InterimTranscriptHash`, `ConfirmationTag`, `GroupState`, `MlsGroupJoinConfig`,
`OwnLeafNodeIndex`, `EpochSecrets`, `MessageSecrets`, `ResumptionPsk`, `EpochKeyPairs`
(HPKE private keys for the current epoch's path), `KeyPackage` (with init and leaf
private keys), `QueuedProposal`, `ProposalQueueRefs`. The leaf signature key is not in
OpenMLS storage when the application supplies its own `Signer` *(spike)*.

So yes, it can be backed by our own encrypted store, and it must be transactional:
one operation writes several labels. The spike keeps OpenMLS's map as the working set
and commits the difference after each request as one batch of records, each
`ChaCha20-Poly1305(store key, storage key || value)` with an HMAC-derived record id as
associated data (`src/store.rs`). Findings:

- One group is one record per label, rewritten wholesale: the `Tree` record is
  rewritten on every commit. memory_storage serialises with JSON (byte arrays become
  number lists, about 3.5×). The production provider uses a compact binary encoding.
- Forward secrecy depends on deletion. Old epoch secrets and key pairs are overwritten
  or deleted records, so the backend must really forget them (§8.3).

### 1.4 Footprint, `unsafe` and advisories

- **Dependencies** (Linux, normal + build): 75 crates, 40 of them new to the workspace:
  openmls (4), tls_codec (2), hpke-rs (3), about 28 libcrux crates (including duplicate
  versions, because libcrux-ml-dsa 0.0.11 is newer than the provider's pins) and
  hax-lib. Dropping `openmls_basic_credential` (the spike signs through libcrux) avoids
  the RustCrypto signature stack (ed25519-dalek, p256, p384, ml-dsa).
- **Compile time** (heavily loaded machine, `-j 4`): dependencies in debug 60 s; clean
  release build 108 s wall, 246 s summed. Largest units: openmls 17 s, libcrux-ml-kem
  16 s, libcrux-hacl-rs 10 s, libcrux-ml-dsa 6 s.
- **Binary size**: a two-party exchange with hybrid identities is 4.88 MB stripped
  versus 0.32 MB for an empty binary (release, thin LTO, one codegen unit): **+4.6 MB**,
  in the crypto worker (and the server, for `PublicGroup`), not the desktop binary.
- **`unsafe`**: openmls is `forbid(unsafe_code)` outside tests; tls_codec, which parses
  all MLS wire data, has none. libcrux's `unsafe` is in libcrux-intrinsics (193 sites,
  AVX2/NEON intrinsics), plus `target_feature` dispatch in libcrux-ml-kem (37) and
  libcrux-ml-dsa (35) (both `deny(unsafe_code)` with local allows), libcrux-secrets (18)
  and a few in platform, traits and aes. Outside libcrux: rayon and crossbeam (OpenMLS
  parallelises TreeKEM), rand_chacha → ppv-lite86 → zerocopy, getrandom and libc, and
  serde_json with memchr, itoa and zmij (memory storage only). None of these parse
  untrusted MLS input. crabgrind appears only under `cfg(valgrind_ct_test)`.
- **`cargo deny check` with `deny.toml`**: licenses, bans (duplicate warnings only) and
  sources pass; **advisories fail**:
  - [RUSTSEC-2026-0329](https://rustsec.org/advisories/RUSTSEC-2026-0329):
    libcrux-hmac-drbg 0.0.1, panic on some output lengths;
  - [RUSTSEC-2026-0330](https://rustsec.org/advisories/RUSTSEC-2026-0330) and
    [RUSTSEC-2026-0331](https://rustsec.org/advisories/RUSTSEC-2026-0331):
    libcrux-kem 0.0.9 panics on short X-Wing keys or seeds. Attacker-supplied key
    packages could crash the crypto worker (a denial of service, contained by the worker
    boundary);
  - [RUSTSEC-2026-0173](https://rustsec.org/advisories/RUSTSEC-2026-0173):
    proc-macro-error2 unmaintained (via hax-lib-macros, build time).

  hpke-rs-libcrux 0.7.0 pins the vulnerable versions exactly (`=0.0.9`), so
  `cargo update` cannot fix them; hpke-rs-libcrux 0.8.0 can, and OpenMLS `main`
  already uses it. Another reason to wait for the next release before adding OpenMLS
  to the main workspace.
- **OpenMLS's own advisories**: 0.9.2 includes the fixes for all published ones
  ([GHSA-94v4-rg25-x625](https://github.com/openmls/openmls/security/advisories/GHSA-94v4-rg25-x625),
  [GHSA-p276-g3f3-wwgh](https://github.com/openmls/openmls/security/advisories/GHSA-p276-g3f3-wwgh),
  [GHSA-gc79-23g3-8g52](https://github.com/openmls/openmls/security/advisories/GHSA-gc79-23g3-8g52)
  and earlier ones).

### 1.5 ML-DSA and ciphersuite migration

- **libcrux**: [libcrux-ml-dsa 0.0.11](https://crates.io/crates/libcrux-ml-dsa)
  (2026-10-07) works today. The spike signs identity certificates with Ed25519 +
  ML-DSA-65 (public key 1952 bytes, signature 3309 bytes). libcrux is pre-1.0 and
  marks crates as verified (hax, HACL\*) or pre-verification per crate
  ([README](https://github.com/cryspen/libcrux)).
- **OpenMLS 0.9.2**: ML-DSA suites exist only in the RustCrypto provider (RustCrypto
  `ml-dsa` 0.1). The libcrux provider signs only Ed25519: `supports()` rejects 0x0052,
  0x0051 and 0x0907 *(spike)*. **OpenMLS `main`**: the libcrux provider signs ML-DSA-44,
  -65 and -87 (#2170, merged 2026-09-10), unreleased.
- **No hybrid signature suite** exists in OpenMLS or in the draft. The MLS combiner
  ([draft-ietf-mls-combiner-02](https://datatracker.ietf.org/doc/draft-ietf-mls-combiner/),
  expired, waiting for the WG chair) amortises PQ protection over two groups.
  `SignatureScheme` is a closed enum in openmls_traits, so a private composite scheme
  would need a fork. Hence: hybrid signatures where Nuschel controls the format
  (identity certificates), plain ML-DSA-44 for MLS leaves once that suite ships.
- **Migration**: OpenMLS parses `ReInit` proposals but offers no public API to create
  one. A group therefore migrates by **generation**: a member creates the replacement
  group (same `ConversationId`, `generation + 1`, new suite) and announces it with a
  signed application message in the old group; members' pins accept exactly that
  generation (§7.2, *spike*), then join (by Welcome, or by external commit with an
  exporter-derived PSK from the old group to bind the two). The old group is closed
  after a grace period. History continues through history keys (§6.6), which are
  independent of the MLS suite. Clients without the new suite cannot join; minimum
  versions (§7.3) make that rare.

### 1.6 Alternatives

- **mls-rs** ([0.57.0](https://crates.io/crates/mls-rs)): post-quantum suites only in
  `mls-rs-crypto-awslc` (feature `post-quantum`, AWS-LC in C); `mls-rs-crypto-rustcrypto`
  0.23 has no ML-KEM. That violates "no C crypto", so it is not an alternative for PQ.
- **Own MLS on top of libcrux**: no; RFC 9420 is large, and OpenMLS is maintained, has
  a security advisory process and publishes fixes (§1.4).

OpenMLS + libcrux is the only pure-Rust MLS stack with a hybrid PQ KEM today. It is
suitable, with the release timing caveat above.

## 2. Threat model

**Protected**: message, file and voice content; typing; read positions; the history
archive; the binding between people and keys.

**Attackers**

- *A malicious or compromised server* (also an operator under compulsion): it controls
  delivery, ordering, ACLs, the directory and the KT log, and the TLS endpoint. It must
  not read content, impersonate members, add members or devices without that being
  visible, replace a group, downgrade a conversation to plaintext, or show different
  people different keys without detection. It can always deny service: withhold,
  delay, drop, disconnect. Withholding and forking are detected (§6.4), not prevented.
- *Network attackers*: TLS (and ALPN in the transcript) stop them; the sticky policy
  and invite fingerprints (extension.md §1) cover downgrades and first contact.
- *Other users*: members see what they are entitled to; a removed member reads nothing
  sent after removal (post-removal history-key rotation, §6.6); a former member keeps
  what they already had.
- *Stolen or lost devices*: an unlocked device reveals its conversations and history
  keys. Locked: protection is the OS credential store's (§3.7). The owner revokes the
  device (§3.6); MLS removal and key rotation shut it out of future messages.
- *Future quantum attackers recording traffic now*: X-Wing protects confidentiality of
  MLS, linking and key delivery. Authentication is classical (Ed25519) in MLS until the
  ML-DSA suite; identity roots are already hybrid, so the migration needs no identity
  change.

**What the server still sees**: which accounts and devices are in which conversation
(the MLS roster, with identity fingerprints); who sends when, and how much; `seq`,
epochs, commit types (add, remove, update: handshake messages are PublicMessage);
file sizes and upload times; KT lookups (who looks up whom); IP addresses; voice
activity and packet sizes; the existence of read-marker and settings blobs and when
they change. Not: content, typing, file contents, read positions, history keys.

**Mumble compatibility limits**: stock clients cannot take part in end-to-end
conversations at all. An end-to-end mode server (the default, extension.md Decision 3)
rejects them at the connection stage. A compatible mode server has no end-to-end
encryption. On stock Mumble servers, Nuschel has no E2EE; a Mumble certificate binding
(§3.9) is a hint, never authentication. Unregistered (guest) users have no account and
no identity, so they cannot be members of end-to-end conversations (open question 12).

**Out of scope**: malware on an unlocked endpoint; traffic analysis beyond the
metadata above; a coerced user.

## 3. Identity

### 3.1 Model

**Decision: a persistent person-level identity, independent of any server, with three
levels.**

```
identity root key (hybrid ML-DSA-65 + Ed25519)       ← fingerprint = the identity
  └─ certifies: device-signing key (hybrid) + credential key (Ed25519), generation g
        ├─ certifies: device d = { device id, key epoch, validity, MLS leaf key
        │                          (Ed25519), X-Wing key, display name }   (hybrid)
        ├─ credential key signs: d's compact MLS credential (§3.3)          (Ed25519)
        └─ signs: device revocations, account bindings, Mumble certificate bindings
```

Implemented in [`crates/identity`](../crates/identity) (`nuschel-identity`): its module
documentation gives every layout, and `test-vectors/identity-v1.txt` the known-answer
vectors. The identity is
`SHA-256(uint8 len || "nuschel/identity-fingerprint/v1" || root public key encoding)`.

- The **root** names the person on every server. Safety numbers, pins and the
  "verified" state refer to it. It signs only device-signing keys, so it is used
  rarely and can stay offline.
- The **device-signing key** (Matrix calls this the self-signing key) lives on the
  user's linked devices. It certifies new devices, revocations and account bindings.
  If a device holding it is lost, the root signs a new one (`generation + 1`) and the
  identity does not change *(spike: `rotating_the_device_signing_key_keeps_the_identity`)*.
- **Device keys** are per device and never leave it: the MLS leaf signature key, and an
  X-Wing key for linking and for key delivery outside MLS.

Compared with the alternatives:

- *One identity key copied to every device* (Signal's linked devices): simplest, but
  any lost device means a new identity, and all verifications on all servers are lost.
  Since one identity spans every server, that cost is much higher here than on a
  single-service messenger.
- *Root on every device*: no recovery key to manage, but a thief can certify devices
  forever; only an identity change helps.
- *Per-device sigchain* (Keybase): every device can add and revoke; flexible but harder
  to reason about and to show; paper keys play the role of our recovery key.

### 3.2 Primitives

| Use | Algorithm | Status |
| --- | --- | --- |
| Identity root, device-signing key | Composite ML-DSA-65 + Ed25519, `id-MLDSA65-Ed25519-SHA512` of [RFC 10073](https://www.rfc-editor.org/rfc/rfc10073) (libcrux), both must verify; the key, not the signature, decides which halves are required | Implemented, checked against the RFC's test vector |
| MLS leaf signatures | Ed25519; then ML-DSA-44 (draft suite 0x0020) | ML-DSA in OpenMLS's libcrux provider on `main` only |
| MLS HPKE | X-Wing | 0.9.2, provisional code points |
| Device linking, key delivery to a device | HPKE with X-Wing, HKDF-SHA-256, ChaCha20-Poly1305 (same libcrux code as MLS) | Works today *(spike)* |
| Stores, archive, files | ChaCha20-Poly1305 / AES-256-GCM, 256-bit keys | Symmetric, PQ-safe |
| KT VRF and tree-head signatures | Ed25519-based (akd) | Classical. Integrity of the hash tree holds; label privacy does not against a quantum attacker |

ML-DSA-65 for the identity (not 44) because the root may live for a decade. MLS leaves
follow the draft's pairing with X-Wing (ML-DSA-44); leaf keys are short-lived and
replaceable.

### 3.3 The MLS credential

A custom credential type (private-use 0xF4E1, listed in the group's required
capabilities) carries a **compact device credential**:

```
struct {
  uint8  kind = 0x03; uint8 version = 1;
  opaque identity[32];               // the identity fingerprint
  uint32 signing_generation;         // which device-signing key signed this
  opaque device_id[16];
  uint32 key_epoch;                  // +1 when the device's keys rotate
  uint64 not_before; uint64 not_after;
  uint16 leaf_algorithm;             // 0x0807 Ed25519: decides key and signature type
  opaque leaf_key[32];               // must equal the MLS leaf signature key
  opaque signature[64];              // credential key, over a context label and the body
} DeviceCredential;                  // 172 bytes
```

**Correction from implementation:** the credential is not signed by the Ed25519 half
of the hybrid device-signing key. RFC 10073 forbids using a component key outside the
composite, so the device-signing certificate certifies a separate Ed25519 *credential
key* that signs only compact credentials. With ML-DSA leaves, a new leaf algorithm
value brings a hybrid credential signature.

The per-person part (root public key, 1986 bytes, and the device-signing certificate,
5439 bytes) is fetched once per contact from the server's directory, checked against
key transparency, and cached. The device's full hybrid certificate (4700 bytes plus the
display name, with the X-Wing key) is in the directory too; a credential is accepted
only with a certificate for the same device and key epoch.

Why not the whole chain in the leaf: the spike first did exactly that. The chain is
16 KB per leaf, a key package 18.7 KB, and the ratchet tree of a 100-member group
1.75 MB (161 KB with compact credentials); every Welcome carries it *(spike: `scale`)*.

Verification, on every Welcome, every commit that adds or updates leaves and every
external join: the credential's signature against the cached device-signing key of
`identity`, the leaf key binding, the device not revoked, and the device present in the
identity's KT record (§5.4). OpenMLS does not validate credentials; the crypto worker
does it before merging a commit or joining. Members see a stranger as a stranger, and a
copied credential fails *(spike)*.

### 3.4 Linking a device

**Decision: magic-wormhole style.** Nuschel is desktop-first and desktops have no
camera, so QR alone is not enough.

1. The new device creates its device keys and opens a linking mailbox on the server
   (a rendezvous id; extension message `DeviceLink*`).
2. The existing device shows a short code (rendezvous id and 8 random digits,
   `42-1234-5678`; a word list like wormhole's can replace the digits in the UI), or
   a QR code with the same content for phones. The user types or scans it on the new
   device.
3. Both run SPAKE2 over the mailbox with the code
   ([`spake2`](https://crates.io/crates/spake2) 0.4.0, RustCrypto, pure Rust; the PAKE
   that magic-wormhole uses, which Nuschel already plans for file transfer on Mumble
   servers). The new device sends its leaf and X-Wing keys with an HMAC key
   confirmation over the transcript (both SPAKE2 messages and the keys); a server that
   swaps keys, or a wrong code, fails the existing device's check.
4. The existing device certifies the new device (device-signing key) and seals a
   provisioning bundle to the new device's X-Wing key with HPKE in PSK mode, the PSK
   derived from the SPAKE2 key. **Correction from implementation:** the X-Wing key
   alone does not authenticate the sender (it travels through the server, which could
   seal its own bundle to it); the PSK shows the new device that the bundle comes
   from the device that showed the code, in this session. The bundle holds the
   identity's directory record, the device-signing private key (if the user chose
   "this device can link others"), the device-sync key (§3.7), pins and verified
   contacts, history keys (§6.6), account bindings. The bundle travels through the
   mailbox.
5. Both devices publish the change to every server's directory and KT. Other devices
   of the user add the new device to existing groups (§6.2).

SPAKE2 is classical, but it only has to resist an active attacker during the linking
minute; the bundle's confidentiality rests on X-Wing, so recorded linking traffic is
safe against a future quantum computer. The spike's bundle with a full chain is 16 KB.

### 3.5 Where the keys live

- **Root private key**: encrypted under a **recovery key** (256 bits, shown once as
  56 Crockford base32 characters in 14 groups of 4, with a 24-bit checksum that
  catches typos, or as a printable sheet; 24 words would need a 2048-word list and are
  left to the UI), stored on each linked device and, optionally, as an
  opaque backup blob on the user's servers. Decrypted only to rotate the device-signing
  key or recover. Without a recovery key the root exists only during setup and is then
  deleted; losing every device holding the device-signing key then means a new identity.
- **Device-signing private key**: on linked devices the user marked "can link devices"
  (default: every desktop; open question 2), in the crypto worker's store.
- **Device keys**: in the crypto worker's store, never exported.
- **As implemented** (open question 1 undecided, so a setting: `offer` by default,
  `require`, `never`): at creation the root is sealed under a fresh recovery key, and
  the key itself waits, sealed under the store key, until the user writes it down
  ("saved": the key is forgotten) or skips it (the recovery blob, and with it the
  root, is deleted). `require` refuses to skip; `never` deletes the root at once.
- **Recovery** when all devices are lost: with the recovery key, a new device restores
  the root, rotates the device-signing key, revokes the old devices; the identity and
  every contact's verification survive. Without it: a new identity, shown to contacts
  as a key change.

### 3.6 Revocation and rotation

- Lost device: any remaining device with the device-signing key signs `RevokeDevice`,
  publishes it to every server's KT, and removes the device from every group (commits).
  If the lost device held the device-signing key, also rotate it with the recovery key.
- A revocation names the device and the key epochs it covers (`through_key_epoch`;
  all of them for a lost device). Device certificates, credentials and revocations are
  valid only under the generation that signed them: after a rotation the remaining
  devices are certified again, and revoked ones simply are not.
- Contacts accept devices only if they are in the identity's current KT record (current
  generation, not revoked), so a revoked device cannot be added back.
- Leaf keys rotate with MLS updates (§6.5); X-Wing device keys rotate yearly or on
  suspicion (new device certificate, same device id, next key epoch, which supersedes
  the older one). Certificates and credentials carry a validity window; how long is
  the crypto worker's policy.
- Inactive devices (not seen for 90 days, open question 8) are removed from groups by
  other members' clients ("Alice's phone was removed: inactive"), which bounds what a
  forgotten device can read.

### 3.7 Persistence and backup

- The crypto worker owns an encrypted store (§8.3). Its store key is in the OS
  credential store: Secret Service on Linux, Keychain on macOS, DPAPI on Windows;
  never in the history database, never on a server. **For now** the key is a 0600
  file in the worker's own directory (`store.key`); moving it to the credential store
  changes only where the worker gets it (`KeySource`: the supervisor hands it over),
  not the sealed files.
- A **device-sync key** (symmetric, shared by the user's devices at linking, rotated on
  revocation by sealing the new key to each remaining device's X-Wing key) encrypts
  per-user blobs on servers: read markers in end-to-end conversations, pins and verified
  contacts, settings. extension.md's `ReadMarker { conversation, seq }` becomes an
  opaque blob in end-to-end conversations.
- **Export**: an encrypted identity file (root + device-signing key under the recovery
  key, plus pins and verified contacts under the device-sync key) for moving to a new
  device without another device online.
- The device keystore holds seeds, never expanded keys: the root key (if present only
  as the recovery blob), the device-signing secret, the device's leaf and X-Wing seeds,
  the certificates and the device-sync key. Each blob is ChaCha20-Poly1305 under a key
  derived (HKDF) from the caller's key, a fresh salt and its purpose; loading re-derives
  every public key and checks it against the certificates.
- No escrow. History is backed up only through history keys held by the user's devices,
  or exported locally.

### 3.8 One identity on every server

- Each Nuschel account is bound to the identity by an `Account { server, account }`
  statement signed with the device-signing key: `server` is the hash of the server's
  identity key, `account` the immutable 128-bit account id (extension.md §3), never
  Murmur's reusable registration number *(spike: `Statement::Account`)*. The server
  publishes the binding in its KT log (§5).
- **As implemented:** `AccountStatement` (kind 0x06, 3467 bytes, context
  `nuschel/account-statement/v1`) names the server by its KT **log id**, which hashes
  the head-signing key (the server identity key) and the VRF key. The `acct:` value
  commits to the statement's hash (format 3, §5.1), the server checks the statement
  before publishing, and clients check it with every lookup
  (`verify_account_record`). So a server cannot present someone's identity as one of
  its accounts: without the identity's signature for that account and log, a copied
  binding verifies nowhere, and "same identity as on X" cannot be faked.
- A server only *claims* that account A is identity I; KT makes the claim the same for
  everyone and lets the user see it. Out-of-band verification (§4) is over the root,
  so verifying someone once holds on every server where their account is bound to
  that root.
- The client keeps one contact per identity. "Alice on servers X and Y" is one person
  when both servers' KT logs bind her accounts to the same root. Pins, verified state
  and nicknames follow the identity.

### 3.9 The Mumble RSA certificate

The identity can sign `MumbleCertificate { sha1 }` for the user's Mumble certificate
(the hash stock Mumble shows), published in KT on Nuschel servers. Nuschel clients then
link a Nuschel identity to a certificate hash, for example to carry friends and
nicknames (keyed by certificate hash today, TODO.md) over to the identity.

What it cannot do: on a stock server, the server only claims each user's certificate
hash, and the binding blob is public. A malicious stock server can present anyone with
Alice's hash and Alice's binding, and it verifies *(spike:
`mumble_binding_is_a_claim_not_authentication`)*. The UI may say "this server says this
is Alice's certificate, which Alice's identity has signed", never "verified". The
binding goes in one direction only: the Mumble certificate never vouches for the
identity, and the RSA key is never used for anything end-to-end.

## 4. Trust on first use and verification

- **Pinning**: on first sight of a contact (first conversation, or first group with
  them), the client pins their identity root fingerprint, globally. A device change
  within the identity is not a key change.
- **Key changes** (the root behind an account changes):

  | Context | Behaviour |
  | --- | --- |
  | Direct conversation, verified contact | Block sending until the user accepts; verified state is lost; red banner |
  | Direct conversation, unverified contact | Inline notice ("Alice's identity changed"), sending continues |
  | Channel, verified member | Inline notice; the lock turns amber until acknowledged |
  | Channel, unverified member | Inline notice in the timeline and member list |
  | Own identity seen with an unknown device or binding (KT self-monitoring) | Blocking alarm with "This wasn't me: revoke" |

- **Safety numbers**: 60 digits, 12 groups of 5. For each person, 30 digits from
  Signal's numeric fingerprint construction (5200 iterations of SHA-512, version
  0x0001) with the 32-byte identity fingerprint as the key and no stable identifier
  (no server identifier: the identity is the same everywhere). Hashing the fingerprint
  rather than the 1986-byte key costs nothing in strength and keeps the 5200 rounds
  cheap. The lower half comes first, so both sides see the same number. A typed number
  is compared exactly: 60 digits, either contiguous or as 12 groups separated by single
  spaces. The QR code holds `NSQ1 || fp_low || fp_high` (the identity fingerprints in
  byte order, 68 bytes); scanning compares both and says which side differs.
- **Verified state** belongs to the identity: verified once, it shows on every server,
  in DMs and in member lists, until the root changes.
- **Automatic verification** (§5) shows "verified automatically" when KT passes, as
  Signal does. Manual verification is stronger: it does not trust the server and its
  auditors.

## 5. Key transparency

### 5.1 Design

**Decision: each Nuschel server runs one verifiable key directory**, modelled on
Signal's [automatic key verification](https://signal.org/blog/automatic-key-verification/)
(2026-08-11), which is based on an earlier IETF KEYTRANS draft and runs in
third-party-auditing mode with Cloudflare and Trail of Bits as auditors.

Labels (VRF-hidden):

- `acct:<account id>` → identity root fingerprint and the account binding's hash;
- `id:<root fingerprint>` → hash of the identity record: root key, current
  device-signing certificate (generation), device certificates, revocations, Mumble
  bindings;
- `name:<registered user name>` → account id.

The records themselves come from the directory API; KT commits to their hashes. The
server publishes a new epoch at most every 10 s while changes are pending, and hourly
otherwise, so that a fresh signed head always exists. Heads are signed with the
server's identity key, the same key extension.md §3 names as the log key; it is pinned
like the server (sticky, carried in invites).

**Implemented** in [`crates/kt`](../crates/kt) (`nuschel-kt`), with these refinements
(2026-11-11):

- **`acct:` carries the roster.** Its value is an `AccountBinding` (value format 1): the
  identity root public key (1986 bytes, so one lookup is enough to check the
  device-signing certificate) and a *roster commitment*, SHA-256 over a context label,
  the hash of the device-signing certificate, and the sorted `DeviceCertificate::hash`
  and revocation hashes. A device change is a new version of `acct:` with the same
  root; TOFU compares roots, never whole values. `roster_for_binding` checks the
  certificates from the directory API against the commitment and builds the identity
  crate's `DeviceRoster`. `id:` keeps its place in the label encoding for a later
  per-identity record (several accounts of one identity on a server, Mumble bindings),
  and the account-binding statement's hash joins the value as a new format once the
  identity crate signs binding statements. `name:` values are format 2 (the account id).
  Every value is `uint16 format || opaque data<0..4096>`, so new formats leave old
  proofs valid, and a client that does not know a format says so.
- **Hourly heads re-sign the current epoch.** akd creates no epoch without changes, so
  "hourly otherwise" means the server signs its current epoch again with a newer
  `issued_at`. Heads of one epoch must name the same root, and `issued_at` never
  decreases across epochs (the server clamps it if its clock goes back). A head for an
  older epoch signed after a newer one is itself evidence (*stale signing*: the server
  was hiding updates from someone), as are two roots for one epoch (*equivocation*).
- **The head-signing key is behind a `HeadSigner` trait** so the server's identity key
  can sign; Ed25519 today. The pinned `LogDescriptor` (suite, signing key, VRF public
  key; 71 bytes) is hashed into a `log_id` that every head names, so a head binds the
  VRF key as well.

### 5.2 Implementation: `akd` behind a narrow interface

| | [`akd`](https://crates.io/crates/akd) 0.13.0 (2026-08-13) | [IETF KEYTRANS](https://datatracker.ietf.org/doc/draft-ietf-keytrans-protocol/) (protocol -06, architecture -11) |
| --- | --- | --- |
| Construction | SEEMless/Parakeet: per-epoch append-only ZK set, VRF, lookup, history and audit proofs | Log tree + prefix tree, binary ladders, monitoring, distinguished entries |
| Production | WhatsApp since 2023; NCC audit (fixes in 0.11); 0.13.0 fixed an append-only bypass | Signal (earlier draft, own Go/Rust code, AGPL) |
| Rust | Yes, MIT/Apache; `akd_core` has `nostd` for clients | No crate on crates.io; experimental `OR13/kt` |
| Dependencies | dalek curves, `blake3` (enable `pure`: no C), protobuf 3, tokio; storage trait (we implement it on PostgreSQL/SQLite) | Would be written by us |
| Standard | No | Architecture in IESG evaluation; protocol a WG draft |

**Decision: `akd`**, isolated in `crates/kt` behind `publish_epoch`, `lookup`,
`history`, `audit` and their client-side verifiers, so a KEYTRANS implementation can
replace it once there is an RFC and a maintained Rust implementation. Writing our own
Merkle prefix tree and VRF from a draft is the larger risk.

**As implemented.** The server uses only the `Directory` trait (`publish`,
`refresh_head`, `latest_head`, `head_at`, `lookup`, `history`, `audit`). `AkdDirectory`
implements it on a `KtStore`, an ordered key-value store with atomic batches; the
server backs it with one table through the storage worker (`kt_records(key, value)`,
point reads and prefix scans; the record layout belongs to `nuschel-kt`, so the worker
never interprets it). Clients use pure functions: `verify_head`, `check_freshness`,
`KnownHeads`, `verify_lookup`, `verify_history` with `monitor_own`, `verify_audit`,
`roster_for_binding`, `ContactTrust::assess`. A backend is a *suite* in the pinned
descriptor; heads, gossip, response envelopes, values and every client result are
independent of it, so a KEYTRANS log would be suite 2 and only its proof encodings and
verifiers would be new.

- **Configuration:** akd 0.13.0 with default features off plus `experimental` and
  `parallel_vrf`; akd's current configuration (`ExperimentalConfiguration`, the one akd's
  main branch deploys; `WhatsAppV1Configuration` exists for WhatsApp's tree) with the
  domain label `nuschel-kt/v1 akd`; BLAKE3 with `pure`; ECVRF-EDWARDS25519-SHA512-TAI
  (RFC 9381), akd's only VRF. The VRF secret is the server's second KT secret: it also
  keys the value commitments and can never change for a log.
- **Findings while integrating akd:**
  1. akd 0.13 does not build without `akd_core/protobuf` (it re-exports `proto`
     unconditionally). The protobuf runtime is linked but never used: proofs travel in
     `nuschel-kt`'s own strict encoding (§5.6).
  2. `akd_core` turns on ed25519-dalek's `legacy_compatibility`, which accepts
     non-canonical (malleable) Ed25519 signatures *for every crate in the same binary*.
     `nuschel-kt` checks `s < ℓ` itself; no other crate linked with it may rely on
     ed25519-dalek for canonical signatures (`nuschel-identity` uses libcrux).
  3. akd's verifier asserts on version 0 and does unchecked arithmetic on epochs.
     `nuschel-kt` rejects such proofs first, caps epochs at 2^48 and runs akd's
     verifiers under `catch_unwind`.
  4. akd's verifier never hashes the label of a membership proof's first sibling entry
     (the root), so 34 bytes per membership proof were malleable (found by the mutation
     test). The decoder requires the root label there.
  5. akd has no absence proof in its public API: "not found" is unauthenticated, a
     failure, never "this account has no key".
  6. akd builds every audit proof against its *latest* tree, so a proof for an older
     epoch walks every path changed since; a range audit cost the span times the later
     changes (238 KB for four quiet epochs at 10,000 accounts). The directory therefore
     stores each epoch's proof while that epoch is the newest (minimal, and valid
     forever because it only links two fixed roots), written atomically with its head,
     and joins them for range audits (§5.8).
  7. The auditor is async and spawns Tokio tasks; `verify_audit` runs it on its own
     thread and runtime, which works inside or outside a caller's runtime.
- **`unsafe` and licences.** akd and akd_core contain only `unsafe impl Send/Sync` on
  types whose fields are thread-safe already. Dependencies with `unsafe` internals:
  curve25519-dalek, BLAKE3's Rust SIMD intrinsics (no C with `pure`), dashmap 5, tokio;
  at build time only, protobuf-codegen's tree (`which`, rustix 0.38). New crates: akd,
  akd_core, ed25519, protobuf 3.7.2 (with protobuf-codegen, -parse, -support),
  value-bag, home: MIT and/or Apache-2.0; dashmap 5.5.3, which: MIT; ed25519-dalek
  2.2.0: BSD-3-Clause; blake3 1.8.7, constant_time_eq: CC0-1.0 or Apache-2.0; rustix,
  linux-raw-sys: Apache-2.0 (LLVM exception) or MIT. `cargo deny` passes, and its
  wrapper rules confine `akd` and `akd_core` to `nuschel-kt`.

### 5.3 Auditing and gossip

- **Gossip inside MLS**: once a day per conversation, a client that has a newer signed
  head for the conversation's server includes it in an MLS application message
  (`kt_head`). Members check it against the heads they know: two different heads for
  the same epoch are, both being server-signed, a proof of equivocation. The client
  shows an alarm and keeps the evidence. Apple's Contact Key Verification gossips heads
  inside messages in the same way.
- **Across servers**: identity records for the same root must agree across servers (the
  device-signing key signs them, so a server can only omit or delay). Clients compare
  generations and revocations between the servers they use and flag a stale server.
- **Third-party auditors** (optional per server): the operator lists auditor keys in
  the `NuschelHello`; clients pin the list (sticky) and then require an auditor
  countersignature on a head no older than 7 days, Signal's bound. Without auditors,
  gossip and monitoring are the defence. Who runs auditors is open question 9.
- **As implemented:** the payload is `HeadGossip { version 1, type 6, SignedHead }`,
  150 bytes, attached when the sender's latest head is newer than any head seen in the
  conversation and a day has passed (`should_gossip`). `check_gossip` compares it with
  the receiver's `KnownHeads`: a contradiction yields `Evidence` (two signed heads, 299
  bytes, checkable by anyone holding the descriptor); a newer head needs an audit proof
  from the receiver's latest head, and a server that cannot produce one has forked; a
  malformed or wrongly signed head is the sender's fault (members cannot forge heads)
  and is dropped.

### 5.4 What clients check, and when

| When | Check |
| --- | --- |
| Every connection | The server's signed head: signature, freshness (< 25 h), append-only since the last head seen (audit proof, or an auditor's countersignature) |
| On connect and every 24 h | Self-monitoring: history proofs for the user's own `acct:` and `id:` labels; any version not made by the user's devices raises the alarm in §4 |
| First contact, new member identity in a roster | Lookup proofs for `acct:` and `id:`; the account's root must equal the credential's `identity` |
| Every new device in a roster | The device certificate's hash is in the identity record at an epoch ≥ its addition. Until included (≤ 1 min) the device shows as "pending"; after that, failure is a warning, and a direct conversation blocks |
| Each connect, for DM contacts and verified contacts | History proofs, so a temporary fake binding cannot come and go between lookups |

With the roster in `acct:` (§5.1), one lookup serves the first-contact and the
new-device rows. Audit responses span at most 4096 epochs; longer gaps are audited in
steps between signed heads (`head_at`).

### 5.5 Key transparency and TOFU

- TOFU pins the root, and KT shows whether the server tells everyone the same thing.
  A KT-consistent rebinding of an account to a new root is still a key change (§4):
  consistent is not the same as benign.
- **Failure UX** (Signal's flow): a first failure is "intermittent", retried within a
  day. A second marks the conversation "Automatic verification unavailable" (amber,
  with the reason) and offers manual verification. Messages stay encrypted to the
  pinned keys, and a manually verified contact stays verified.
- **Which error means what** (`KtError::class`, `Assessment::surface`):

  | Errors | Class | Shown |
  | --- | --- | --- |
  | Bad head signature, head of another log, stale head (> 25 h) or from the future, rollback (an older head replayed), invalid lookup or history proof, response for another label, malformed message or value, unauthenticated "not found", certificates not matching the roster commitment or not verifying under the bound root | Failure | First: nothing, retry within a day. Again: "Automatic verification unavailable" |
  | Equivocation, stale signing (both with signed evidence), failed audit between two signed heads, a history that contradicts an earlier verified one, a version that went back | Server misbehaviour | Alarm; the evidence is kept |
  | A version of an own label with a value the user's devices did not make | Own-key alarm | Blocking "This wasn't me: revoke" |

  A failed audit counts as misbehaviour because the server signed both heads and a
  correct server can always connect them. A KT-consistent rebinding is a key change
  per §4: a verified direct contact blocks sending until accepted (the verified pin is
  kept until then), a verified channel member turns the lock amber, otherwise an inline
  notice re-pins. A history in which another root came and went shows a notice too.

### 5.6 Wire formats

Every message starts with `uint8 version = 1, uint8 type`. Integers are big-endian,
every variable-length field has a length prefix and a maximum, enum and option tags
accept only defined values, trailing bytes are an error, and counts are checked against
the remaining input before anything is allocated. Each value has one encoding: parsed
messages re-encode to the same bytes (the fuzz target checks this).

| Type | Message | Size |
| --- | --- | --- |
| 1 | `LogDescriptor`: suite, signature scheme, signing key, suite parameters (VRF key) | 71 bytes |
| 2 | `SignedHead`: log id, epoch (≤ 2^48), root, `issued_at` (ms), signature over a context label and the fields | 148 bytes |
| 3, 4 | `LookupResponse`, `HistoryResponse`: head, label, suite proof | ≤ 256 KiB, ≤ 8 MiB proof |
| 5 | `AuditResponse`: epochs, the roots in between, suite proof | ≤ 4096 epochs, ≤ 32 MiB |
| 6 | `HeadGossip` | 150 bytes |
| 7 | `Evidence`: kind, two heads | 299 bytes |

Suite 1's proof encodings (node labels, membership and non-membership proofs, VRF
proofs, update, history and append-only proofs) are specified in
`crates/kt/src/akd_backend/codec.rs`. Known-answer vectors: `crates/kt/tests/vectors/kt-v1.txt`.

### 5.7 Privacy

- **Labels are hidden by the VRF.** A label's position in the tree is
  `VRF(label, version)`; only the server can compute it, and a client can only check a
  VRF proof for a label it already knows. Other users appear in proofs as pseudorandom
  256-bit node labels, so the tree cannot be enumerated or joined with a list of
  account ids, fingerprints or names without asking the server, which sees and can
  rate-limit every lookup.
- **Values are hidden by commitments** keyed by the VRF secret; sibling hashes reveal
  nothing about other users' keys.
- **A lookup reveals** about others only the approximate directory size (path length)
  and, across epochs, whether neighbouring subtrees changed; about the looked-up label,
  its value, version count and the epoch of its latest change. A history reveals every
  past value and when it changed (served only for one's own labels and contacts). Audit
  proofs reveal how many leaves each epoch added (one per new label, two per change),
  not whose.
- **Post-quantum:** label and value hiding rest on the discrete logarithm (recovering
  the VRF secret also opens the commitments), and heads are Ed25519. Tree integrity
  rests on BLAKE3. A hybrid head signature is a new signature scheme in the descriptor.

### 5.8 Measurements

Release build, in-memory store, 4 Tokio worker threads, 2018-byte values
(`crates/kt/tests/perf.rs`):

| | 10,000 accounts | 100,000 accounts |
| --- | --- | --- |
| Initial publish of all accounts (one epoch) | 1.1 s | 12 s |
| Store size | 30k records, 27 MB | 300k records, 266 MB |
| Publish 1 change / 100 updates (incl. storing the audit proof) | 1 ms / 13 ms | 1 ms / 17 ms |
| Lookup response (avg; 2.2 KB of it value and head): generate, verify | 6.8 KB: 0.7 ms, 0.5 ms | 7.8 KB: 0.9 ms, 0.7 ms |
| History of 3 versions: size, generate, verify | 15.6 KB: 1.8 ms, 1.3 ms | 17.7 KB: 1.9 ms, 1.5 ms |
| Audit of an epoch with 1 change: size, verify | 1.8 KB, 0.7 ms | 2.3 KB, 0.8 ms |
| Audit of an epoch with 100 updates: size, verify | 66 KB, 7 ms | 99 KB, 14 ms |
| Audit of the initial import (epoch 0 → 1): size, verify | 660 KB, 41 ms | 6.6 MB, 0.39 s |
| Head: verify | 0.05 ms | 0.05 ms |

A client audits each epoch it skipped at about 0.5 to 1 KB per changed leaf (one
leaf per new label, two per change; each costs the siblings along its path that it
does not share with the others). On a quiet server
that is negligible; on a busy one (an epoch every 10 s) a client offline for a day
would download megabytes, which is when auditor countersignatures (§5.3) should replace
client audits. Clients never audit from epoch 0: the first head is trusted on first
use, like the server's key.

## 6. MLS integration

### 6.1 One group per conversation, bound to it

- One MLS group per `ConversationId`; direct conversations are groups of the two
  identities' devices.
- Group id = `ConversationId || generation`. The group context carries the
  **conversation binding** extension (private-use 0xF4E0): server identity key hash,
  `ConversationId`, generation, and the conversation policy (history: `members` or
  `since_join`; membership: `server` or `members`; policy admins by identity). Required
  capabilities list the binding extension and the Nuschel credential type, so no member
  without them can be added *(spike)*.
- Wire format: handshake messages as PublicMessage, application messages always
  PrivateMessage (`MIXED_PLAINTEXT_WIRE_FORMAT_POLICY`) *(spike)*.

### 6.2 Membership: driven by ACLs, authorised by members

The server knows who may read (`can_read`, extension.md §5) and cannot add anyone
itself: it holds no member key, and anything it could do is visible *(spike)*.

- The server is listed in the group's `external_senders` with its identity key. When
  `can_read` changes, it sends **external proposals** (Add with a key package from its
  directory, Remove) as signed PublicMessages; this puts the request in the transcript.
- Members' crypto workers **commit** proposals after checking: credential and KT (§3.3,
  §5.4), and for Adds that the conversation's membership mode allows server-driven
  adds. The server names an online member in each task to avoid commit storms; if two
  commit, epoch fencing picks one (§6.3).
- **Membership modes**:
  - `server` (channels): adds and removes follow the server. Every roster change is
    shown, with the identity.
  - `members` (direct conversations, private groups): only removals follow the server;
    adds need a commit by a policy admin acting deliberately. A direct conversation never
    takes anyone but the two identities' devices.
- **Channel membership = subscribers, not everyone who could read.** With default ACLs
  every registered user can Enter every channel. So a device becomes a member when it
  subscribes (opens the conversation), by external commit (§6.6), and leaves after
  30 days unsubscribed. `can_read` bounds it; the server removes a device that loses it.
- **Removal timing**: the server stops delivering to a device the moment `can_read`
  is lost (it controls delivery), and the next online member commits the removal. Until
  then the removed device holds the epoch's keys but receives nothing.
- **Own devices**: a device of an identity that is already a member is added
  automatically, by the user's other device or by external commit; members accept it if
  it is in the identity's KT record.
- Policy changes (history, membership mode, admins) are GroupContextExtensions commits
  by an admin; the server's per-channel settings are mirrored, and on a mismatch members
  follow the group context.

### 6.3 Delivery service in `nuscheld`

The server tracks each group's public state with OpenMLS `PublicGroup` from the
PublicMessage handshakes *(spike: `src/ds.rs`)*:

- **Epoch fencing**: a commit is accepted only for the current epoch, first come first
  served; others get `NuschelError { code: stale_epoch, epoch, head_seq }`. The committer
  merges its own commit only after the server's ack (never before), and clears it on
  rejection.
- **Validation before fan-out**: framing, signatures against the sender's leaf, proposal
  rules. Membership and confirmation tags need epoch secrets the server does not have;
  members check those *(spike)*.
- **Application messages only for the current epoch**, so receivers keep no past-epoch
  secrets (`max_past_epochs = 0`: forward secrecy); a sender behind a commit catches up
  and resends with the same idempotency key *(spike)*.
- **One stream per conversation, one `seq`**: commits, proposals and application
  messages share the conversation's `seq`, so a page `after(seq)` replays everything
  in order. Welcomes are not in the stream.
- **GroupInfo with every commit**: the committer uploads the new epoch's signed GroupInfo;
  the server checks its epoch and serves it, with its own copy of the tree, to external
  joiners and to Welcome recipients.
- **No tree in Welcomes**: the ratchet tree extension is off (the spike still uses it).
  Joiners fetch the tree from the server, authenticated by the tree hash in the
  GroupInfo. At 1000 members the tree is 1.5 MB (X-Wing).
- **Key package directory**: per device, up to 50 one-time key packages plus one last
  resort package; claims are rate-limited per account and consume a package; the server
  checks signatures and suite on upload.
- **Welcome mailbox** per device, kept until fetched (bounded age).
- **Commits are kept** as long as any member device may still need them to catch up:
  retention and tombstones (extension.md §6) apply to application content, never to
  commits inside the catch-up window (90 days, the inactive-device bound).

**Where MLS changes docs/extension.md**:

1. Handshake and application messages share the per-conversation `seq` space; history
   pages include commits (marked as such), and clients cannot skip them.
2. A commit is accepted only for the group's current epoch (`stale_epoch` error with
   the head), and application messages only for the current epoch.
3. New messages in the 0x4E00 block: `MlsCommit { request_id, conversation,
   idempotency_key, commit, group_info, welcome?, recipients }` → `MlsAck { seq, epoch }`;
   `MlsKeyPackageUpload` / `MlsKeyPackageClaim`; `MlsWelcome` (push) and `MlsWelcomeAck`;
   `MlsJoinInfo` (GroupInfo + tree); `MlsExternalProposal` (push); `DeviceLink*`
   (mailbox); `SelfBlobPut`/`Get` (§3.7); `Kt*` (`kt/1`).
4. `ChatSend.content` is the MLS PrivateMessage; ChatSend gains an optional `archive`
   field (§6.6). Edits and deletions are envelopes inside MLS (the server cannot see
   which message they refer to); purging content on the server is a separate
   server-visible request, checked against the stored author account.
5. `ReadMarker` becomes a self blob, and typing an MLS application message.
6. The server never drops commits within the catch-up window.

### 6.4 The application payload: per-sender ordering

A server-assigned `seq` lets a malicious server reorder, withhold or fork history per
member (extension.md §3). Every MLS application message therefore carries a Nuschel
**envelope**, inside the ciphertext:

```
struct {
  uint8  version;
  uint8  kind;               // chat, edit, delete, reaction, file, typing,
                             // history_key, history_grant, kt_head, generation, ...
  opaque device_id[16];      // the sending device (also implied by the MLS leaf)
  uint64 counter;            // per (device, conversation), +1 per durable envelope
  opaque prev[32];           // SHA-256 of this device's previous durable envelope
  opaque seen[32];           // SHA-256 of the newest envelope (any sender) this device
                             // had processed when it sent
  opaque idempotency_key[16];// equals ChatSend's: the server cannot swap it
  uint64 sent_at;            // sender clock, milliseconds
  opaque archive_hash<0..32>;// hash of the archive record sent alongside (§6.6)
  opaque body<V>;
} Envelope;
```

Typing indicators are not durable: they carry no counter and are never stored.

Receivers keep `(counter, hash)` per sender device and conversation:

- `counter = last + 1` and `prev` matches: in order.
- `counter > last + 1`: **gap**. Messages from that device are missing. Fetch the
  stream after the last known `seq`. If they never arrive, show a persistent marker
  ("2 messages from Alice's laptop were not delivered by the server").
- Same counter with different content, or `prev` not matching a known envelope:
  **fork**. The server showed different histories (or the device is compromised): alarm,
  both versions kept as evidence.
- `seen` names an envelope this client never saw: another member saw a message withheld
  from this client. Fetch, then mark a gap. This catches cross-sender forks.

MLS covers the rest: replays fail (keys are deleted after use), and members on different
commit histories end up in different epochs and cannot read each other, which shows as
a fork as well.

### 6.5 Keeping post-compromise security

The spike shows that a stolen state heals only after the victim's own update. Each
device commits a self-update when it sends after 7 days without one, after 1000 own
messages, after linking or revoking a device, and when the user asks. The server's
committer selection prefers members with stale leaves.

### 6.6 History for new members

User decision: new members see earlier history by default (`members`); a per-channel
setting restricts it (`since_access`). MLS gives a member keys only from its join epoch
*(spike: `external_join_reads_from_its_join_on`)*.

Options:

| Option | Verdict |
| --- | --- |
| Hand joiners past epoch secrets (or exporter secrets) | No. Keeps every epoch secret alive forever, which defeats MLS forward secrecy for live traffic too |
| Re-encrypt the archive for each join | No. Cost proportional to history on every join; someone must hold and decrypt everything anyway |
| **Per-conversation history keys** | **Yes** |

**History keys.**

- Each conversation with history has a chain of 32-byte history keys `H_k`. Every
  durable message is sent twice: live, as the MLS application message (forward secrecy,
  post-compromise security), and as an **archive record**:
  `AEAD(HKDF(H_k, message nonce), envelope)` plus the sender device's leaf-key signature
  over the record. The MLS envelope carries the record's hash, so a sender cannot show
  members one text and the archive another. The server keeps the record as the
  history body (`ChatSend.archive`) and needs the MLS message only until every member
  device has fetched it.
- **Rotation**: a fresh `H_{k+1}` after every removal (sent in the first epoch the
  removed device cannot read) and at least every 30 days, as an MLS application message
  (`history_key`).
- **Grants**: when a device joins, the member that added it (for external joins, the
  first online member who sees the join) sends a `history_grant` MLS application message
  with the history keys covering the retained archive. Own devices get the identity's
  keys at linking (§3.4).
- **Who may grant**: any member device, automatically, but only as the conversation
  policy in the group context allows: `members` grants every admitted member the
  retained keys; `since_join` grants nothing except to the identity's own new devices.
  The policy changes only by an admin's commit, so the server cannot widen it.
- **A removed member keeps** what is on its device: its local history, and history keys
  up to the removal. It gets no new keys, the server stops serving it pages, and rotation
  protects everything after the removal.

**What users give up compared with "only since joining"**:

- Forward secrecy for archived content: an unlocked or compromised member device reveals
  every archived message of its conversations (with the server's ciphertexts), not
  only what is still on the device.
- Every admitted member (admitted under the ACLs, by a member's commit, visibly) reads
  everything retained. A careless admin exposes the whole archive; a malicious server
  still cannot add anyone silently.
- Post-compromise security for the archive comes only from rotation: a device
  compromised in period k reads periods ≤ k, plus any period it is granted while
  compromised.

`since_join` conversations still get history keys, for the identity's own devices, so a
user's new laptop shows their history.

### 6.7 Large channels and performance

Measured, release build, one core per operation, compact credentials *(spike: `scale`)*:

| Members | Suite | Add n−1: time, commit | Welcome | Tree | Update commit (fresh tree): size, create, process |
| ---: | --- | --- | ---: | ---: | --- |
| 10 | X-Wing | 30 ms, 32 KB | 32 KB | 20 KB | 17 KB, 2.9 ms, 1.9 ms |
| 100 | X-Wing | 131 ms, 289 KB | 284 KB | 161 KB | 126 KB, 17 ms, 14 ms |
| 300 | X-Wing | 0.99 s, 853 KB | 840 KB | 468 KB | 363 KB, 88 ms, 50 ms |
| 1000 | X-Wing | 10.5 s, 2.8 MB | 2.8 MB | 1.5 MB | 1.2 MB, 200 ms, 77 ms |
| 100 | X25519 | 38 ms, 45 KB | 50 KB | 34 KB | 8.9 KB, 41 ms, 3.5 ms |
| 1000 | X25519 | 2.3 s, 442 KB | 491 KB | 339 KB | 83 KB, 89 ms, 29 ms |

A 100-byte message is 250 bytes in both suites. Joining a 1000-member group takes
245 ms. The update commits were measured on a tree just built by one bulk add, whose
parent nodes are blank, so the path is encrypted to O(n) recipients. It shrinks towards
O(log n) as members commit their own updates, and removals blank paths again. X-Wing
ciphertexts are 1120 bytes against 32, so X-Wing commits are about 14× larger.

Consequences:

- Compact credentials and no tree in Welcomes (above).
- The server batches ACL-driven changes into one task and names one committer.
- Channel membership is subscribers, not every possible reader (§6.2).
- **End-to-end channels are capped at 1000 member devices** by default (server setting,
  open question 6). Above it the server refuses to create the group; the channel stays
  transport-encrypted with a red lock, or the operator splits it.

### 6.8 Files

As extension.md §8 requires: each file gets a fresh random 256-bit key (never
convergent encryption). The client encrypts in 64 KiB chunks (ChaCha20-Poly1305 STREAM
construction: chunk index and last-chunk flag in the nonce or AAD), uploads to the blob
store under its own `upload_id`, and sends `{upload_id, key, hash of the ciphertext,
size, name, type}` in a `file` envelope (and in the archive record, so history grants
include files). Downloads are authorised by `upload_id` and `can_read`, never by the
hash. Thumbnails are separate encrypted blobs. The server sees sizes and timing;
clients never trust a server MIME type.

### 6.9 Voice and media: SFrame with MLS exporter keys

- [RFC 9605](https://www.rfc-editor.org/rfc/rfc9605.html) §5.2:
  `base_key = MLS-Exporter("SFrame 1.0 Base Key", "", AEAD.Nk)` per epoch; KID =
  context, sender leaf index and the epoch's low E bits; receivers drop an epoch when a
  newer one with the same low bits arrives. The spike checks that every member derives
  the same base key and that it changes per epoch *(spike)*.
- **Voice group**: per voice channel, an MLS group of the devices present (and
  listening), separate from the channel's chat group (readers and speakers differ in
  Mumble). Devices join by external commit on entering and are removed on leaving;
  voice channels are small, so the churn is affordable.
- **Transport**: Opus frames are SFrame-encrypted before the Mumble UDP/TCP voice
  framing, so on Nuschel servers in end-to-end mode, voice is end-to-end encrypted on
  today's transport (the server routes and never decodes anyway), and the same frames
  work later over WebRTC. SFrame's counter gives replay protection per sender; the
  voice badge turns green only when every participant is in the voice group.
- Whispers to users use the direct conversation's exporter; shouts to other channels
  are not end-to-end encrypted and are marked (open question 7).

### 6.10 Offline delivery

Devices catch up with `after(seq)` pages (commits included). Key packages (with a last
resort package) let others add an offline device; Welcomes wait in its mailbox. A device
offline longer than the catch-up window (90 days) is removed, and on return rejoins by
external commit or is re-added; the gap is shown. Messages that can no longer be
decrypted (expired epochs, lost state) show as explicit gaps, never silently.

## 7. Downgrade protection

### 7.1 Server policy

As extension.md §1: end-to-end mode (the default for new servers) rejects stock clients
at ALPN with a displayable `Reject`; compatible mode is the operator's explicit choice.
Capabilities `mls/1` and `kt/1` are in the `NuschelHello`. The sticky record is keyed by
server address, later by the server identity key, so certificate rotation does not reset
it. Invites carry the server identity-key fingerprint and policy. A server that drops
`mls/1` fails the connection; it never continues in a weaker mode.

### 7.2 Per-conversation pins

The client's own state decides, not the server's advertisement *(spike: `src/pins.rs`)*:

- Once a conversation has an MLS group, it is pinned: server identity, `ConversationId`,
  generation, active.
- Plaintext for a pinned conversation is refused.
- A Welcome or external commit for a pinned, active conversation is refused unless its
  generation was announced inside the current group (migration, §1.5).
- After being removed, rejoining is allowed and shown ("You were added back by …").
- A binding for another server is refused.
- Pins are part of the crypto worker's store and of the provisioning bundle, so a new
  device inherits them.

### 7.3 Minimum client versions

The `NuschelHello` carries `minimum_client` and `recommended_client` with a grace date,
advisory as extension.md says. For MLS, the binding requirement is stricter: a group's
required capabilities and suite exclude clients that cannot take part, and migrations
(§1.5) announce a deadline. Clients below the minimum see an update notice; after the
grace date the server rejects them.

### 7.4 Lock states

| Indicator | Meaning |
| --- | --- |
| Grey, pending | Connecting, negotiating, or KT pending for a new device (≤ 1 min) |
| Red, open | No end-to-end encryption: compatible-mode server or stock server (transport only) |
| Green, closed | MLS group established, every member credential verified, KT consistent |
| Green, closed, check mark | The same, and every member identity verified out of band (direct conversations: the contact) |
| Amber, closed, "!" | Encrypted, but something needs attention: an unacknowledged key change of a verified member, KT unavailable or inconsistent, a gap or fork |

Voice has its own indicator: green only when SFrame is active for every participant.
Chat-only encryption never turns the server badge green (docs/architecture.md).

## 8. Implementation plan

### 8.1 Crates and processes

| Component | Responsibility | Must not acquire |
| --- | --- | --- |
| `crates/e2ee` (`nuschel-e2ee`) | Identity types and encodings (`tls_codec`), compact credentials, certificate verification, safety numbers, binding and pins, envelope, archive records, SFrame key schedule. No I/O | Network, database, UI |
| `apps/crypto-worker` (`nuschel-crypto-worker`) | Owns identity, device and MLS state; OpenMLS + libcrux; the encrypted store; history keys; verification policy | Network, server credentials, history database |
| `crates/kt` (`nuschel-kt`) | `akd` wrapper: server log and client verifiers | UI |
| `crates/protocol` | `nuschel.v1` MLS, KT, linking and blob messages, with limits | |
| `apps/server` (`nuscheld`) | Delivery service (`PublicGroup` per group), epoch fencing, key package directory, Welcome and linking mailboxes, KT log, external proposals | Member keys |
| `crates/storage` | Server tables: group stream, GroupInfo, trees, key packages, mailboxes, KT state | MLS private keys |

- cargo-deny wrappers: `openmls*` only in `nuschel-e2ee`, `nuschel-crypto-worker` and
  `nuscheld`; `akd` only in `nuschel-kt`.
- The crypto worker follows the worker contract in SECURITY.md: versioned, bounded IPC,
  its own store directory only, no network, sandboxed like the storage worker. The
  session hands it MLS messages and gets back plaintext envelopes and outgoing
  messages; the storage worker gets plaintext for local history (as today), never keys.
- SECURITY.md's exception table gains libcrux (SIMD intrinsics) in the crypto worker and
  server.

### 8.2 Order of work

1. **Phase 3a, identity offline**: `nuschel-e2ee` identity types from the spike, with
   compact credentials and `tls_codec` encodings; recovery key and identity export;
   Settings → Identity.
2. **3b, crypto worker**: encrypted store, OS credential store integration, IPC, crash
   tests.
3. **3c, server identity**: account ids, directory, device linking (SPAKE2 mailbox),
   KT log and client checks, self-monitoring, safety numbers and QR, contacts by identity.
4. **4a, delivery service**: group registry with `PublicGroup`, shared stream, epoch
   fencing, key packages, mailboxes, join info, external proposals.
5. **4b, direct conversations**: two identities' devices, envelopes with counters, lock
   states, key-change UX. Ship only on an OpenMLS release with final code points.
6. **4c, channels**: server-driven membership, subscriber membership, external joins,
   committer selection, multi-device adds.
7. **4d, history keys** and archive records, self blobs, files.
8. **4e, voice**: SFrame on the Mumble voice path, voice groups.
9. **4f, migration**: group generations, first exercised by a test migration X25519 →
   X-Wing, then the ML-DSA suite.

### 8.3 The crypto worker's store

A small pure-Rust log-structured store: AEAD records as in the spike, one appended batch
per request with `fsync`, compaction by rewriting into a new file and renaming. No
SQLite in the worker. Forward secrecy needs real deletion: compaction runs after epoch
changes, and the store key rotates weekly (the spike's `rekey`), so deleted records left
on flash become unreadable once the old key is gone from the OS credential store.

### 8.4 Tests

- **Multi-client simulation**: the spike's `sim` grown into a deterministic simulator
  of N clients and a delivery service, with random schedules of sends, commits, joins,
  removals, reconnects, crashes (between store commit and ack, between ack and merge),
  and reordering. Invariants: members converge on the epoch authenticator; nothing is
  accepted twice; non-members read nothing; history keys reach exactly the policy's
  recipients.
- **Malicious server suite**: a delivery service with attacks (forged commit, external
  join with a stranger or a copied credential, group replacement by Welcome, plaintext
  for a pinned conversation, withheld and reordered messages, forked histories, stale or
  split KT heads, widened history policy). Each must be detected by the clients, with
  the specified UX. The spike already has the first five.
- **Interop**: state written by one OpenMLS version loads in the next; groups with mixed
  versions; libcrux against the RustCrypto provider (`openmls_rust_crypto` as a
  dev-dependency) for X-Wing HPKE and ML-DSA; RFC 9420 and draft test vectors.
- **Fuzzing** (the existing `fuzz/` workspace): MLS message decoding and
  `process_message` on a fixture group, Welcome staging, key package validation,
  credential and certificate decoders, envelope and archive record decoders, KT proof
  verification, and the server's submit path.
- **NixOS VM tests**: server and three desktops: a direct conversation, a channel, an
  external join, device linking, server restart, offline catch-up, a stock client
  rejected in end-to-end mode, and an ALPN-stripping proxy.

## 9. Dependency notes

- New, in the crypto worker and server only: openmls, openmls_traits,
  openmls_libcrux_crypto, openmls_memory_storage (tests only once our provider exists),
  tls_codec, hpke-rs, the libcrux family, libcrux-ml-dsa; for KT: akd and akd_core
  (blake3 with `pure`, protobuf 3, dalek curves); for linking: spake2. All MIT or
  Apache-2.0.
- `cargo deny` currently fails on three libcrux advisories pinned by
  openmls_libcrux_crypto 0.4.0 (§1.4). Do not add ignores; wait for the release that
  moves to hpke-rs 0.8.
- `rayon` (from OpenMLS) starts a thread pool in the crypto worker; its CPU limit must
  allow for it.

## 10. Open questions for the user

1. **Recovery key**: required at identity creation (as Matrix pushes it), or offered and
   skippable? Without one, losing all devices that hold the device-signing key means a
   new identity. *(Implemented as a setting, offered and skippable by default.)*
2. **Device-signing key placement**: on every linked desktop (default here), or only on
   devices the user marks? Phones later?
3. **Key transparency backend**: `akd` now (recommended), or wait for KEYTRANS and
   implement it ourselves?
4. **Channel membership**: subscribers with lazy external joins (recommended), or every
   device with `can_read`?
5. **History grants**: should `members` channels grant history to every admitted member
   automatically (recommended, matches the decision), or should private channels require
   an admin to approve each grant?
6. **Size cap** for end-to-end channels: 1000 member devices? And above it: red lock, or
   refuse to create such channels on an end-to-end server?
7. **Voice**: a voice group per channel of present devices (recommended)? Are shouts to
   other channels allowed without end-to-end encryption, or blocked?
8. **Inactive devices**: removed after 90 days?
9. **Auditors**: should the Nuschel project run a public auditor that servers can opt
   into, or leave auditors to operators?
10. **Release timing**: agree to ship persistent end-to-end groups only on an OpenMLS
    release with the draft's code points and fixed libcrux crates (prototype on 0.9.2)?
11. **Ciphersuite**: the draft's AES-128-GCM X-Wing suite (recommended), or OpenMLS's
    ChaCha20 suite 0x004D, which is not in the draft?
12. **Guests**: unregistered users cannot join end-to-end conversations, so on an
    end-to-end server they read no text. Accept, or give guests ephemeral identities
    shown as "guest, unverified"?

## Phase 3 as implemented (2026-11-11)

Identity and key transparency work end to end without MLS. What the implementation
settled, beyond the refinements marked "as implemented" above:

- **Processes.** `apps/crypto-worker` (`nuschel-crypto-worker`) owns every identity
  secret and key-transparency decision. It runs in the shared bubblewrap sandbox
  (`crypto_sandbox_command`: its own directory at `/data`, no network), one per
  profile, shared by all connections; the session (`nuschel_session::identity`) only
  moves bytes between it and the server and never links `nuschel-kt` (cargo-deny
  confines `nuschel-kt` to the worker and `nuscheld`, `nuschel-identity` to them and
  `nuschel-kt`). The IPC is `nuschel_ipc::crypto`: typed JSON with base64 binaries,
  every field bounded (§8.1). `nuschel-e2ee` (§8.1) is not needed yet:
  `nuschel-identity` carries the identity formats.
- **The worker's store** (§8.3, for phase 3): the sealed `DeviceKeystore`, a JSON state
  sealed under the store key (purpose 5: pinned logs, `KnownHeads`, our bindings and
  monitoring checkpoint, contacts' `ContactTrust`, verified identities), and the
  pending recovery key; each file written atomically (synced temporary file,
  rename). The log-structured store for MLS state comes with phase 4.
- **Signing.** The worker signs account statements (one per server and account,
  reused while valid) and renews this device's certificate 30 days before it expires
  (validity one year). Devices without the device-signing key cannot bind accounts yet
  (multi-device publishing comes with linking).
- **The server** (`nuscheld`, docs/server.md): the directory on its `kt_records` table
  through its own storage worker; records behind each binding (`R ‖ account ‖
  SHA-256(value)`, and `C ‖ account` for the current one) in the same table; one task
  serves every request in order, so a lookup never pairs a new value with an old
  record. Publications must verify under the root and carry the identity's statement
  for this account and log; epochs every `publish_interval` (10 s) while bindings
  wait, heads re-signed every `refresh_interval` (an hour). Keys from credential files
  (`LoadCredential`), or generated once in the state directory.
- **Who may ask** (§5.7, the review's guidance): lookups by any connection for any
  registered account of the server (by registration number or account id); histories
  only for the owner and contacts, which the server knows as direct-conversation
  partners and users in the same channel right now (`[kt] history = "members"` opens
  them to everyone); audits and heads for anyone; each with a per-minute rate limit.
  A verified contact without a direct conversation is checked by lookup only, with
  the version check of `ContactTrust` (a transient binding between two lookups is not
  seen then).
- **The schedule** (§5.4): on every connection and hourly, the server's head (audited
  from the last accepted one; a span the server finds too large is halved, with
  `KtHead` for the epoch in between). On connect: publish our binding (the server
  skips an unchanged value), self-monitor (again after 2, 4, 8 … s while the binding
  waits for its epoch), and histories of direct and verified contacts; then daily.
  Contacts are looked up when their profile or a direct conversation with them opens.
- **Self-monitoring baseline.** The first check starts at the newest version of our
  account's history that carries a value this device made; earlier versions (another
  identity of the same person, or whoever held the account before) are not judged.
  Every later version must be ours, or the blocking own-key alarm is raised and kept
  until the user acknowledges it.
- **Pins.** The worker pins each server's log descriptor (by the sticky record's
  `host:port` key) on first contact; a different descriptor later is misbehaviour
  ("the log changed"), never accepted, and only an explicit reset (`ForgetServer`)
  clears it. `kt/1` itself is sticky like every capability (§7.1).
- **Key changes** (§4, §5.5): a verified contact's new identity is
  `KeyChanged { blocking: true }` with the old pin kept until the user accepts; an
  unverified contact's is re-pinned and shown as changed until acknowledged. The
  desktop shows both in the profile and as the server's notice; MLS-level blocking of
  sends comes with phase 4.
- **Cross-server identity.** Verification is stored per identity fingerprint with the
  server (and its label) where it was made; on another server whose log binds the same
  identity (with the identity's statement for that account), the contact shows
  "verified (same identity as on X)", by server label, never by name.
- **Gossip** (§5.3) is ready: `MakeGossip` and `CheckGossip` produce and check
  `HeadGossip` against the worker's `KnownHeads` and keep `Evidence`; carrying it in
  MLS application messages comes with phase 4.

**MLS integration points** for phase 4: the crypto worker gains the OpenMLS state (and
the log-structured store of §8.3); credential verification uses the device roster that
`verify_account_record` already returns; the KT check of new devices in a roster
(§5.4) reuses the contact lookup; head gossip rides in application messages; key-change
blocking moves from the profile into the conversation's send path; and device linking
publishes the new roster with the existing `KtPublish`.
