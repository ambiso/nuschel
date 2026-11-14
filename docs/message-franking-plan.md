# Message franking for reports

Planning document, 2026-11-13, on main (`b8ca83ab`), with the owner's
decisions of 2026-11-14 (§11). Nothing here is implemented. Part of #272;
it grants no approval beyond that issue.

Goal: a recipient reports an end-to-end encrypted message or a sticker. The
moderator verifies that the reported content is what the sender sent, who
sent it, and where and when the server delivered it. Unreported messages
stay opaque to the server.

## 1. Summary

- **Commitment:** each content envelope carries a random 32-byte franking
  key `k_f`. The sender puts `com = HMAC-SHA256(k_f, context ‖ envelope)` in the
  MLS PrivateMessage's `authenticated_data`. MLS encrypts the envelope and
  signs and authenticates `com` with it.
- **Server binding:** at fan-out the server computes
  `tag = HMAC-SHA256(K_s, conversation ‖ seq ‖ time ‖ account ‖ com)` and
  sends `(seq, time, account, tag)` with the row, live and in history pages.
- **Receipt:** recipients recompute `com` and refuse a message whose `com`
  fails, so every message they show is reportable.
- **Report:** the client sends the envelope, `k_f`, `com` and the server
  fields to the conversation's server. The server checks `tag` with `K_s`
  and `com` with `k_f`, and its moderators read the verified plaintext.
- **Primitives:** HMAC-SHA256 and SHA-256 from libcrux (`libcrux-hmac`
  0.0.9 and `libcrux-sha2`, already in the tree), in `nuschl-e2ee`, inside
  the existing libcrux boundary (SECURITY.md). No new crate.

This is the design of [GLR17] and Messenger's Secret Conversations [FB16],
placed where MIMI puts it for MLS: `franking_tag` in the PrivateMessage AAD,
a hub MAC over it with the accepted time [MIMI].

## 2. Threat model

| Party | Attempt | Defence |
| --- | --- | --- |
| Sender | unreportable content | receipt check (§4.3) |
| Sender | other content for the moderator | `com`; files/1 `R` |
| Sender | another member's name | MLS signature; `account` in `tag` |
| Sender | edit or delete later | recipients keep openings (§6) |
| Reporter | altered content | `com` |
| Reporter | other sender, time or place | `tag` |
| Reporter | replay into another report | `tag` names conversation and `seq` |
| Reporter | quote out of context | franked context; `seq` gaps show |
| Server | read unreported messages | `com` hides the envelope |
| Server | fabricate a report | open: it holds `K_s` |
| Server | suppress reports or tags | open: it receives them |
| Moderator | read beyond a report | holds no MLS or history keys |

HMAC-SHA256 under a fresh 256-bit key is a PRF, so `com` hides the
envelope; its collision resistance binds `k_f` and the envelope. Franking
evidence is the server's own statement: a server can frame its own users to
itself, and a moderator who distrusts the operator gains nothing from it.

Moderators of a conversation: holders of the channel's moderation right
(`Policy.moderators_delete` and Write, docs/extension.md) for channels, and
server administrators for direct conversations (§11). They act with existing
tools: moderator deletion (tombstone), kick, ban, and the server's MLS
removals (docs/e2ee.md "Phase 4c").

What MLS already gives: every PrivateMessage is signed by the sender's leaf
key over the content, `authenticated_data` and the group context (RFC 9420
§6.1). KT binds that leaf key to an identity. What franking adds:

| Property | MLS signature | Franking |
| --- | --- | --- |
| Binds content, sender | yes | yes |
| Binds `seq`, time | no | yes |
| Proves server delivery | no | yes |
| Opening | signature, content, group context | `env'`, `k_f`, `frank` |
| Verifiable by | anyone with the KT record | the issuing server |

## 3. What gets committed

```
k_f   = 32 random bytes, a new field of envelope version 2
env'  = the envelope's encoding with archive_hash empty
        (the form the archive record seals, docs/e2ee.md §6.6)
com   = HMAC-SHA256(k_f, "nuschl/frank/v1" ‖ log_id[32] ‖ group_id[20]
                         ‖ credential[172] ‖ u32 len(env') ‖ env')
aad   = u8 1 ‖ com[32]                       (MLS authenticated_data)
tag   = HMAC-SHA256(K_s[key_id], "nuschl/frank-server/v1" ‖ u32 key_id
                    ‖ log_id ‖ conversation[16] ‖ u64 seq ‖ u64 time_ms
                    ‖ account[16] ‖ com)
```

- `env'` holds kind, device, counter, `prev`, `seen`, idempotency key,
  `sent_at`, `k_f` and the body: text, reply target, file references
  (files/1 key, pieces root, length), and for a sticker its `FileRef` and
  `StickerInfo` (sticker plan §3.1).
- `log_id` and `group_id` (conversation ‖ generation) bind the server and
  conversation; `credential` is the sender's 172-byte Nuschl device
  credential (identity fingerprint, device, leaf key).
- Fixed-length fields and one length prefix keep the input unambiguous.
  MIMI also keys its HMAC with a value carried inside the content.
- A resend after `STALE_EPOCH` reuses the stored envelope, so `com` and the
  idempotency answer stay the same.
- `K_s` is a 32-byte server key with a `key_id`, kept with the server's
  signing key. It rotates yearly; old keys stay while history they tagged is
  retained.

### 3.1 Why HMAC suffices here

| Construction | Commits to | Here |
| --- | --- | --- |
| HMAC-SHA256 [GLR17] | key, message | `com` |
| AES-GCM, ChaCha20-Poly1305 | nothing | salamanders [DGRW18], [LGR21] |
| Key hash in the header [ADGKLS22] | key | files/1 `C` |
| UtC, HtE transforms [BH22] | key; HtE also context | later, for files/1 |
| CTX [CR22] | key, nonce, AD | needs the AEAD's internal tag |

The opening of a message is its plaintext and `k_f`, so MLS's AEAD
(AES-128-GCM in suite 0x004F) needs no key commitment for franking. Openings
that are keys (attachments, stickers) rely on files/1's key commitment
`C` inside the root `R`. That covers the attachment attack [DGRW18]
found in Messenger. Terms follow draft-irtf-cfrg-aead-properties.

## 4. Where it lives

### 4.1 Sender (crypto worker)

The worker draws `k_f`, builds the envelope, computes `com`, sets it with
OpenMLS `MlsGroup::set_aad` and encrypts. `com` covers the content kinds
(chat, edit, delete, reaction, file). Typing, history keys, grants and other
control kinds carry an empty AAD.

### 4.2 Server (`apps/server/src/mls.rs`)

The delivery service already reads the PrivateMessage framing to fence
epochs. It reads `com` from the same framing, refuses an application
message whose AAD is neither empty nor `u8 1 ‖ com`, stores `com` beside
the row, and computes `tag` when it assigns `seq`. `ChatEvent` and history
pages gain `frank { key_id, time_ms, account, com, tag }`; the history row
already has `seq`, time and author account. Size: 33 bytes per message on
the wire to the server, 32 more stored.

### 4.3 Recipient (crypto worker)

After MLS decryption the worker checks: AAD present, `com` recomputes from
`k_f`, the MLS-authenticated sender's credential and `env'`, and the
server's `account` equals the authenticated author account (an existing
check). A failure is a decryption gap, as for a bad signature. The worker
keeps `(env', com, frank)` for each shown message in its sealed store; the
storage worker keeps the plaintext as today. The recipient cannot check
`tag`; a bad `tag` only makes that server's own report check fail.

Archive pages carry the same `frank`. A later joiner checks `com` against
the archive record's envelope, so archived messages are reportable too.

## 5. Report flow

1. The user picks "Report" on a message, a reason (abuse, illegal content,
   spam, other) and up to 1,000 characters of text, and may tick earlier
   messages of the conversation as context (§11).
2. The client sends `MessageReport { request_id, conversation, reason,
   text, items[] }`, each item `{ seq, frank, credential, env' }`, to the
   conversation's server over the existing session, in the moderation block
   (0x4E68–0x4E7F).
3. The server checks, per item: `tag` under `K_s[key_id]`; `com` from
   `k_f`; the envelope decodes, its device matches the credential, and the
   credential belongs to `account`'s KT roster; and the reporter reads the
   conversation now or read it at `seq`. One failed item refuses the report.
   The check needs no stored row, so reports survive purges and retention.
4. The server stores the verified report in its database (storage worker):
   reporter, items, reason, text, time. Moderators get a count; the
   reported account learns nothing from the report itself.
5. A moderator's client fetches the report and renders `env'` bodies as
   any message: inert typed content, images through the content worker.
   For attachments it fetches the object, checks `R` from the revealed key
   and decrypts locally; the server checks `R` only.

Bounds: 20 items, 1.5 MiB per report, 10 reports an hour per account, 1,000
open reports per server (oldest dismissed reports dropped first).

### 5.1 Which server receives it

| Report about | Goes to | Because it holds |
| --- | --- | --- |
| E2EE message or sticker | its Nuschl server | `K_s`, the users, the tools |
| `history/1` message or sticker | its server | the plaintext row |
| Feedback, crashes | `reports.nuschl.eu` | content-free reports (#205) |

Message and sticker reports stay with the conversation's operator.
reports.nuschl.eu holds no `K_s` of other operators, has no authority over
their users, and #205 excludes message content. Servers run by the Nuschl
project show their reports in triage.nuschl.eu (D3).

## 6. Edits, deletions, attachments, stickers, history

- **Edits** (encrypted edits are still to be built): an edit is a durable
  envelope naming its target's envelope hash, franked like any other. A
  report of an edited message carries the original and every edit up to the
  shown version (§11). Recipients keep superseded openings for 30 days.
- **Deletions:** the delete envelope is franked; the server purge removes
  the row and `com`. Recipients keep the opening of a deleted message,
  hidden, for 30 days so that "Report" stays available (§11). Server
  verification needs no row.
- **Attachments:** `env'` carries the files/1 key and pieces root. The
  report reveals that one key; `R` binds key, length and ciphertext, so one
  object opens to one file.
- **Stickers:** a sent sticker is a files/1 object of the conversation
  with a fresh key (sticker plan §3.1). A sticker report is an attachment
  report: it reveals that object's key and the franked envelope, so the
  moderator sees the image, `StickerInfo`, the sender and the time.
  Moderator deletion releases the object. In E2EE conversations each copy
  has its own key and object, and the server cannot link copies, so each
  copy needs its own report. A pack shared in a conversation is a set of
  objects there and is reported the same way.
- **History:** archive pages carry `frank`; reports work for messages read
  from the archive. Messages from envelope version 1 carry no `k_f`; their
  reports name `seq` only and the server records them as unverified.
- **Migration:** new groups require the `franking/1` group feature as a
  required capability. Existing groups adopt it by a group-context commit
  once every member supports it.

## 7. Deniability

Nuschl conversations are already non-deniable. Each MLS message carries the
sender's Ed25519 leaf signature, KT binds the leaf to an identity, and the
archive record is signed by the same key. A member can show any third party
a signed message today.

Franking keeps that state. The tag is a MAC only the issuing server can
check, so the franking material alone convinces only that server. A report
moves plaintext and an opening to the server, which a recipient can already
do with a screenshot; franking adds verification.

Deniable reporting needs deniable message authentication. Asymmetric
message franking [TGLMR19] gives moderator-only verifiability and
forgeability for everyone else. MLS's per-message signatures already
transfer authentication, so AMF on top of MLS gains nothing; it needs a
replacement for MLS sender authentication. The owner decided on no
deniability (§11). Source tracing of forwarded messages (Hecate [IAV22]) is
out of scope.

## 8. Stock Mumble and plaintext servers

- **Stock Mumble:** the server receives every text message in plaintext and
  Mumble has no report message. Nuschl offers local ignore and hides
  "Report" (§11). Plugin data (#93) is unauthenticated, so there is nothing
  to verify.
- **`history/1` servers (compatible mode, no E2EE):** the server stores
  plaintext rows with `seq` and author. A plaintext `MessageReport` names
  `seq`s and the server reads its own rows; franking adds nothing there.

## 9. Slices

1. `nuschl-e2ee::frank`: `com`, `tag`, report-item verification, test
   vectors. Pure, no I/O.
2. Envelope version 2 with `k_f`; AAD through `set_aad`; receipt checks and
   the sealed store of openings in the crypto worker; `franking/1` feature.
3. Server: `com` from framing, `K_s` with key ids, `frank` in `ChatEvent`
   and pages; protocol fields in `crates/protocol`.
4. `MessageReport`: server verification, bounds, reports table, moderator
   listing; desktop report dialog and moderator view.
5. Attachments and stickers (files/1 objects) in reports.
6. Edits and deletions, with the encrypted edit and delete envelopes.
7. Plaintext reports on `history/1` servers; triage.nuschl.eu for
   project-run servers (D3).

## 10. Tests

- Vectors: `com` and `tag`; `libcrux-hmac` against RFC 4231.
- `com` fails for a change in any field: body, `k_f`, credential, group id,
  log id, length prefix. `tag` fails for any of conversation, `seq`, time,
  account, `com`, `key_id`.
- Receipt: missing AAD, wrong `com`, a credential of another member,
  version 1 envelope in a `franking/1` group: each a gap.
- Server: tags in live fan-out equal tags in pages; a resend keeps `com`;
  reports verify after purge and after key rotation; forged tag, `com`,
  `seq`, time, account or conversation refused; a reporter outside the
  conversation's readers refused; a removed member reports a message from
  before its removal; bounds at and over each limit.
- Loopback: two clients, a server and a moderator client as separate
  processes with real workers: send, report, verified view with author;
  edit then report the original; delete then report; a later joiner reports
  an archived message.
- Attachments: the correct key verifies; a second key for an object built
  to open under two keys is refused.
- Stickers: a report verifies through `R`; deletion releases the object.
- Privacy: the server's rows, logs and diagnostics for unreported messages
  contain ciphertext, `com` and metadata only.
- Moderator client renders report bodies through the content worker.
- `history/1` report by `seq`; stock Mumble shows no "Report".

## 11. Decided (owner, 2026-11-14)

1. **No deniability** ("this was already decided"): messages are
   non-deniable, as MLS signatures make them. Reports carry franking
   material and no MLS signatures.
2. **Franking is required** in every E2EE group (`franking/1`); recipients
   refuse messages that fail the franking check.
3. **Context:** up to 19 earlier messages chosen by the reporter.
4. **Deleted messages** stay reportable for 30 days; openings are kept
   hidden for that time.
5. **Edits:** a report carries the original and every edit.
6. **Moderators:** channel moderators for channels, administrators for
   direct conversations.
7. **Retention:** servers keep reports for one year after closing.
8. **Stock Mumble:** no "Report" option.

## 12. Open decisions

1. **Destination:** message and sticker reports to the conversation's
   server; reports.nuschl.eu stays for #205 (recommended).
2. **Server binding:** HMAC-SHA256 with a server key (recommended; MIMI's
   `server_frank`) or a signature with the server's KT key, which anyone can
   verify and which makes server statements transferable.
3. **Project-run servers:** their reports in triage.nuschl.eu
   (recommended), or in the desktop moderator view alone.
4. **`K_s` rotation:** yearly, old keys kept while tagged history exists.

## References

- [GLR17] Grubbs, Lu, Ristenpart: Message Franking via Committing
  Authenticated Encryption, CRYPTO 2017.
- [FB16] Facebook: Messenger Secret Conversations, technical whitepaper.
- [DGRW18] Dodis, Grubbs, Ristenpart, Woodage: Fast Message Franking: From
  Invisible Salamanders to Encryptment, CRYPTO 2018.
- [TGLMR19] Tyagi, Grubbs, Len, Miers, Ristenpart: Asymmetric Message
  Franking, CRYPTO 2019.
- [LGR21] Len, Grubbs, Ristenpart: Partitioning Oracle Attacks, USENIX
  Security 2021.
- [ADGKLS22] Albertini, Duong, Gueron, Kölbl, Luykx, Schmieg: How to Abuse
  and Fix Authenticated Encryption Without Key Commitment, USENIX Security
  2022.
- [BH22] Bellare, Hoang: Efficient Schemes for Committing Authenticated
  Encryption, EUROCRYPT 2022.
- [CR22] Chan, Rogaway: On Committing Authenticated-Encryption, ESORICS 2022.
- [IAV22] Issa, Alhaddad, Varia: Hecate: Abuse Reporting in Secure
  Messengers with Sealed Sender, USENIX Security 2022.
- [MIMI] draft-ietf-mimi-protocol-06 §5.4.1 (franking), §5.9 (abuse
  reports).
- RFC 9420 (MLS) §6; draft-irtf-cfrg-aead-properties.
