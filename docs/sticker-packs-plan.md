# Sticker packs: global dedupe locally, conversation copies remotely

Planning document, 2026-11-14, stacked on the emoji picker (#250, `80ed9812`).
Nothing here is implemented. Part of #253; it grants no approval.

Owner direction (2026-11-13/14): one local copy per sticker; the image travels
as a per-conversation attachment with the pack id and key, and tapping it shows
the pack with an Install button, as in §9 of the clean-room sticker interface
spec (kept outside the repository), which the importer also follows. Nuschl's
Terms of Service, accepted once, cover uploads.

## 1. The model

| Where | What | Keyed by |
| --- | --- | --- |
| Client | one copy of each sticker image and its decoded frames, whatever conversation or server it came from or went to | SHA-256 of the image (`digest`) |
| Client | the library: packs the user imported, created or installed | `pack_id` |
| Server | a files/1 kept object per sticker per sender and conversation | the object's random key and root |
| Server | encrypted packs, uploaded on first use where a sticker is sent | `pack_id`; the key stays with clients |

A sticker message carries the image as an ordinary files/1 attachment
(docs/files.md: fresh random key, Padmé, BEP 52 root) and a `StickerRef` with
the pack id and pack key. A resend in the same conversation references the
sender's existing object (files.md §4.3); another conversation gets a new
object under a new key. A receiver holding the `digest` downloads nothing.
Tapping a sticker fetches the pack from the server the message arrived on.

**A server learns** sizes and timing of the image objects, which it cannot
link across E2EE conversations, and pack ids: who uploaded and who fetched
which pack, and when, as the external service's CDN learns. Pack keys, titles
and images stay with clients. A `history/1` server reads the messages, keys
included.

## 2. Packs

### 2.1 Manifest and library

```proto
message PackManifest {
  uint32 version = 1;             // 1
  string title = 2;               // <= 64 graphemes
  string author = 3;              // <= 64 graphemes
  uint32 cover = 4;               // object index of the cover
  repeated Sticker stickers = 5;  // display order; entry i is object i + 1
  bytes import_tag = 6;           // §5.2, imported packs only
}
message Sticker {
  string emoji = 1;   // required: one emoji of #250's table
  string alt = 2;     // optional, <= 120 bytes
  uint32 kind = 3;    // PNG, APNG, WebP, animated WebP (sniffed)
  uint32 width = 4; uint32 height = 5; uint32 frames = 6; uint32 ms = 7;
  bytes digest = 8;   // SHA-256 of the image, kept byte for byte
}
```

The desktop keeps a `sticker_packs` table in the storage worker (settings.json
cannot gain keys, #250) and images under `stickers/<digest>` in the profile;
the web client uses IndexedDB. Received stickers outside the library share
that store, bounded to 256 MiB, least recently used first. Imports (§5) and
packs created from image files pass the content worker checks (§5.4) and a
review.

### 2.2 Pack encryption

Objects `0..=N+1`: `0` the manifest, `1..=N` the stickers (`1 <= N <= 200`),
`N+1` an optional cover.

```
PRK    = HKDF-Extract(salt = "nuschl stickers/1", IKM = pack_key[32])
k_i    = HKDF-Expand(PRK, "nuschl stickers/1 object" || u16 i, 32)
ek_i   = HKDF-Expand(k_i, "nuschl stickers/1 aead", 32)
c_i    = SHA-256("nuschl stickers/1 commit" || u16 i || k_i)
inner  = u32 len || bytes || zeros to Padmé(4 + len)      (as files/1)
blob_i = "NST1" || nonce[12] || c_i ||
         AEAD(ek_i, nonce, inner, aad = "NST1" || u16 i || c_i)
h_i    = SHA-256(blob_i)                                   the object id
index  = h_0 || h_1 || ... || h_{N+1}                      public
pack_id = SHA-256("nuschl stickers/1 pack" || index)
```

- `pack_id` fixes the public index, so receivers verify every blob from any
  source and servers check membership with no key. A pack is its hash;
  editing makes a new pack under a new random key.
- Opening compares `c_i` in constant time, then opens the AEAD. With the key
  committed and the nonce in the blob, a ciphertext opens to one plaintext:
  the commitment fix of Albertini et al. (USENIX Security 2022) and
  Bellare–Hoang (EUROCRYPT 2022), as files/1's `C`. Image attachments rely
  on files/1's root `R`; message franking (#274) binds the sender.
- AEAD and HKDF come from `libcrux-aead` 0.0.10 and `libcrux-hkdf`, in the
  tree; its ChaCha20-Poly1305 is verified HACL\* code, constant-time on
  wasm32, and `libcrux-aes` needs its verification checked (D1). The crate
  `nuschl-stickers` (no I/O) runs in the native driver; the content worker
  gets plaintext images only; the web client needs libcrux on wasm32.

## 3. Sending, showing and installing

### 3.1 The reference

```proto
message StickerRef {
  bytes digest = 1;                       // the local cache key
  string emoji = 2;  string alt = 3;      // one emoji of the table; <= 120 B
  bytes pack_id = 4; bytes pack_key = 5;  // fetch and open the whole pack
  uint32 index = 6;                       // the sticker's place in the pack
}
```

A sticker message is one `FileRef` (files.md §4.2) plus `StickerRef`, with
no text. Receivers validate every field as the spec's §4.3 does.

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Mumble |
| --- | --- | --- | --- |
| Image | files/1 kept object in the conversation | the same | the inline legacy image |
| Carrier | `ChatSend` attachment plus a `sticker` field | `FileRef` and `sticker` in kind 13's `ChatBody` (#216), group feature `stickers/1` | `nuschl/sticker/1` over message 26 (#93) |
| Legacy text | the sender's inline image, for stock clients | none | the visible `TextMessage` |
| Pack | the server's pack store | the same | none (D6) |

On stock servers the visible message is the sticker's first frame as an
`<img>` from the content worker's `EncodeImage`, within the server's limit,
then the emoji, linked to the annotation by `msg_id` and text digest (#216
§2.4).

### 3.2 Upload on first use

On a server with capability `stickers/1`:

1. The image uploads as a kept object in the conversation, and the message
   follows at once.
2. `StickerHave {pack_id}` returns the pack objects the server holds or has
   blocked; the client uploads the rest in the background with
   `StickerPut` (write-once), manifest first, with the index, the terms
   record (§4.1) and, for imports, the import record (§5.2).

Pack objects travel on files/1's second TLS connection (docs/files.md), also
through the web relay, and live in the server database, one row per object. Any
authenticated user may fetch by hash; reading needs the key from a message. The
server checks each blob's hash against the index, its size and header.

### 3.3 Receiving and installing

1. A `digest` already in the local store shows at once; otherwise the client
   downloads the attachment, checks `R`, decrypts, checks the image's
   SHA-256 against `digest` and stores it. On stock servers a cached
   `digest` shows the original, animation included; otherwise the inline
   still.
2. Tapping the sticker fetches the manifest from that server, checks it
   against `pack_id` and `c_0`, and shows the pack: title, author and every
   sticker, fetched as they scroll into view.
3. "Install" fetches the remaining objects, verifies each, and adds the pack
   to the library. A pack still uploading shows its present stickers; a
   removed one shows "Pack unavailable on this server".

The content worker (on the web, the content WASM worker) decodes every image,
shown at 160 logical px; animation plays while visible, holds its first frame
under reduced motion and plays on hover or focus. The accessible name is
"Sticker: <alt text, or the emoji's name>"; copying copies the emoji.

## 4. Terms of Service

### 4.1 Acceptance and record

Uploading stickers is covered by Nuschl's Terms of Service, accepted once.
The client shows them in the first-run dialog (beside #205's feedback
choice), and on existing installs the first time the user sends a sticker or
adds a pack (D9). Until they are accepted the Stickers tab shows them in place
of the library:

> **Nuschl terms**
> Stickers you share must be yours to share. People you send them to can
> use them in Nuschl. [Read the terms]
> [Accept] [Not now]

The text is `terms/nuschl-terms-1.en.txt`, shipped in the client, with id
`nuschl-terms/1` and hash SHA-256 of its canonical bytes (UTF-8, NFC, LF, no
trailing spaces). The client stores the accepted id, hash and time, and sends
them with the first pack upload to each server, which records them with the
account and its own time. Nothing else is recorded.

### 4.2 The stickers section, version 1

1. **Your confirmation.** By adding or sending stickers in Nuschl you confirm
   that you created their images and text, or that everyone holding rights
   in them has allowed you to license them on these terms, and that they,
   used as these terms allow, do not infringe anyone's rights or break the
   law. A link to a pack does not give you that right.
2. **Servers.** You grant the operator of every Nuschl server a pack or
   sticker is sent on, by you or by people who received it, a simple
   (non-exclusive), worldwide, royalty-free, perpetual and irrevocable right
   to host, store, reproduce, distribute, publicly display and make it
   available through that server, including the technical changes these
   need: format conversion, resizing, compression, padding and encryption.
3. **Recipients.** You grant everyone who receives a pack or its stickers
   the same kind of right to use, display, install and send them on within
   Nuschl. Each operator may pass these rights on to its users.
4. **Irrevocable.** You can delete a pack from your library and ask an
   operator to stop hosting it. Copies people received, and their rights,
   stay.
5. **Removal.** Each operator decides what its server hosts and may refuse
   or remove any pack or sticker at any time, for example after a report or
   a legal notice.
6. **Other sources.** Imported packs come from third-party sources. Nuschl
   is independent of them, and they do not endorse Nuschl or its importer.
7. **Your moral rights.** You remain the author. EU and German law keep some
   rights with you: to be named as the author, to object to distortion of
   your work, and to recall rights if a work stops reflecting your
   convictions. You agree that stickers appear in messages with no author
   line, and that a pack shows the author you entered.
8. **What is recorded.** A server you upload a pack to records your account
   there, which version of these terms you accepted and when. Its operator
   keeps this as proof of the licence while it hosts your packs and for
   three years after, based on these terms and the interest in defending
   legal claims (GDPR Art. 6(1)(b) and (f)). You can ask that operator for
   a copy.

Best-effort wording by a non-lawyer; worth a legal check before launch.

Sources: GitHub ToS D.3–D.5, D.7; Discord Terms "Your content"; CC BY 4.0
§2(a), §2(b)(1), §6(c); UrhG §§13, 14, 31, 31a, 32, 42; OLG Frankfurt 6 U
174/23 (2024); BGB §195; GDPR Art. 6, 13, 15; DSA Art. 16, 17, 20.

## 5. Importer for external pack links

### 5.1 Flow

1. "Add pack" takes a pasted link in either form of the spec (§8) and
   validates both values; the client registers no URL scheme handler. The
   help line reads "Nuschl is independent of the apps pack links come from."
2. It derives the tag (§5.2) and looks in the library, then asks the
   connected Nuschl servers (D3). A hit fetches and checks (D2) that
   server's pack and installs it.
3. A miss says "Downloads the pack once", fetches and checks manifest and
   images (§5.3, §5.4), shows a review (missing emoji are chosen in #250's
   picker) and adds the pack to the library under a fresh pack key.

### 5.2 Dedupe before any external request

```
tag    = HMAC-SHA256(ext_pack_key,
                     "nuschl stickers/1 import tag" || ext_pack_id)
wrap   = HKDF(IKM = ext_pack_key, salt = "nuschl stickers/1 import",
              info = "wrap" || ext_pack_id, 32)
record = AEAD(wrap, {pack_id, pack_key, ext manifest and image SHA-256s})
```

A pack's first upload to a server stores `{tag, record}` for imports;
`StickerImportLookup {tag}` returns up to four records. A server learns equal
imports and their times; an operator holding a link learns who imported it.

A first importer can poison a tag, and only the external CDN can prove a
pack's contents. The recommended check (D2) fetches the external manifest and
two random images (3 requests of up to 202) and compares hashes; a mismatch
is reported (§6) and imported afresh.

### 5.3 Polite fetching

- Only on the user's import: no background fetching, prefetch or crawling.
- HTTPS to the configured CDN host only, no cross-host redirects, a plain
  `Nuschl/<version>` User-Agent, the user's proxy setting.
- One import at a time, two requests in flight, 100 ms between starts, each
  distinct file once; 15 s connect, 30 s per file, reads capped at 10 KiB
  and 301 KiB.
- 429 and 503 wait for `Retry-After` (at most 10 minutes); other errors back
  off 1–32 s with jitter, four attempts per file; three failed files in a
  row stop the import. 403 and 404 mean "not found".
- A cache by tag and index lets a resume refetch nothing (kept 7 days).

### 5.4 Untrusted manifest and images

External blobs are decrypted in `nuschl-stickers` as the spec's §3 describes,
decrypt only: HKDF and HMAC from libcrux, AES-256-CBC from RustCrypto `aes` and
`cbc` (already in the lockfile). The manifest is parsed after its MAC check,
within 10 KiB: invalid UTF-8 becomes U+FFFD, Cc and Cf characters go, and
duplicate or out-of-range indexes reject the pack. Images go to the content
worker one at a time through new typed requests, `StickerCheck` (kind, size,
frames, duration, a 96 px preview) and `StickerFrames` (frames at display size),
for imports, created packs and every received sticker. Results are bound to
their import or message, request and generation.

| Bound | Value |
| --- | --- |
| Input | <= 300 KiB; PNG, APNG or WebP by magic bytes; declared MIME ignored |
| Header dimensions | <= 512 x 512, checked before decoding |
| Frames | <= 150 and <= 3 s, decoded one at a time, two resident |
| Time | 2 s wall per image, under the worker's memory limit |
| Reply | frames at <= 160 px, under the 12 MiB IPC frame |
| Desktop cache | 64 MiB of decoded frames, least recently used first |

**External hosts:** the importer recognises the external link host and contacts
the external CDN host (spec §2, §8). Their names appear only as two constants in
the `external_pack` module and its tests, with a switch that turns the importer
off (D4); identifiers, files and UI use neutral names.

## 6. Moderation

- **Image report:** an attachment report under message franking (#274): it
  reveals that object's key and the franked envelope, so the moderator sees
  the image, `StickerRef`, the sender and the time; `R` proves the key opens
  that object. #274's sticker row changes to this (D11).
- **Pack:** the franked envelope carries the pack key, so moderators open the
  pack on their server and verify each blob by `c_i` and the index.
- **Removal:** moderator deletion tombstones the message and releases its
  image (files.md §1.5). Removing a pack deletes its blobs and blocks its
  id, so re-uploads fail and clients show "Pack unavailable on this server".
- **Limits:** in E2EE conversations each copy of a removed image needs its
  own report; blocked pack ids cover installs; a `history/1` server can
  also refuse removed digests.
- **Uploaders** get reasons and one appeal (DSA Art. 17, 20); three upheld
  removals in a year suspend uploads there. Anyone can hide a pack locally.

## 7. Storage and limits

Images are stored once per sender and conversation (50 stickers of 60 KiB in 20
conversations: about 60 MiB, against 3 MiB for one copy each), count toward the
sender's files/1 quota and follow their messages' retention (files.md §1.5).
Packs are stored once per server and stay 180 days after their last upload check
or fetch (D5).

| Limit | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Formats | PNG, APNG, WebP still and animated | the content worker's decoders |
| Animation | <= 3 s, <= 150 frames, >= 20 ms per frame | bounded decode |
| Manifest | <= 64 KiB encrypted | 200 entries with alt text |
| Pack | <= 64 MiB of blobs | 201 x 301 KiB padded |
| Packs | 1 GiB and 100 packs first uploaded per user, 20 GiB per server | operator cost |

`services.nuschl.stickers` in `nix/module.nix` sets `enable` (default true),
`maxPackBytes`, the pack quotas, `packRetentionDays` and `importLookups`.

## 8. The picker (#250)

The Stickers tab replaces "Stickers are coming soon." with the library: pack
tabs like the emoji category row (Recent, the packs, "+" Add pack: link or
files), cells keyed by grid position with §3.3's names and arrow keys, and
search over each sticker's emoji names, keywords and alt text. Click sends and
closes, Shift+click sends and stays open. Empty state: "No sticker packs yet"
and "Add pack". A received sticker opens its pack on a click, with "Install"
and "Report". The web picker matches; adding from links and files is desktop
only (D8).

## 9. Slices

| # | Slice | Needs | Size |
| --- | --- | --- | --- |
| S1 | `nuschl-stickers`: manifest, pack format, links, tag, vectors | — | M |
| S2 | Content worker `StickerCheck` and `StickerFrames`, bounds | — | M |
| S3 | Desktop library, digest store, picker tab, create from files, terms | S1, S2 | M |
| S4 | Importer: dedupe, fetching, review | S3 | L |
| S5 | files/1 kept objects end to end (docs/files.md §8) | — | L |
| S6 | Sending and showing stickers on the three paths, resend reuse | S3, S5, #93, #216 | M |
| S7 | nuschld pack store, first-use upload, import lookups, NixOS | S1, S5 | M |
| S8 | Pack view and Install | S6, S7 | M |
| S9 | Reports through #274, pack removal and blocks | S7, #274 | S |
| S10 | Web: library, picker, rendering, sending, pack view | S1, S8 | M |

## 10. Tests

- Vectors for keys, commitments, `pack_id`, tags and records; a blob in
  another slot or outside the index, and a GCM ciphertext built for two
  keys, are refused. The content worker refuses a declared 65535 x 65535
  PNG, a 10,000-frame APNG, long WebP, truncated files and MIME mismatches.
- A synthetic external pack on a local CDN fixture in a VM: a library hit
  makes 0 requests, a server hit 0 or 3, a miss one per file, a resume none.
- One nuschld VM, two conversations: the same sticker yields two image
  objects with different keys; a resend adds no object; a receiver holding
  the digest downloads nothing; the pack uploads once per server; tapping
  fetches the manifest, Install the rest; wrong hashes, roots or `c_i` are
  refused; a removed pack shows unavailable.
- Uploads wait for accepted terms; the server records id, hash and time.
- Desktop smoke with AT-SPI names; stock Mumble shows the inline image and
  Nuschl one sticker.

## 11. Open questions (the spec does not say)

1. Does the external CDN use a publicly trusted certificate, or the private
   CA the spec mentions (then pinned for that host only)?
2. Its rate limits, `Retry-After` use and expected request headers.
3. Whether its terms allow third-party clients to fetch packs.

## 12. Decisions

Decided (owner, 2026-11-13/14): per-conversation image copies with global dedupe
on the device; Terms of Service accepted once; packs fetched by id from the
message's server, with tap to view and Install. Open:

1. **Pack AEAD:** ChaCha20-Poly1305 (recommended) or AES-256-GCM (libcrux).
2. **Dedupe hit:** verify with 3 external requests (recommended) or none.
3. **Lookup scope:** all connected Nuschl servers (recommended) or one.
4. **External hosts:** two constants plus an off switch (recommended), or a
   configuration file.
5. **Pack retention:** 180 days after last use (recommended; keeps pack ids
   out of messages) or counted from referencing messages.
6. **Stock servers:** tapping shows packs already on the device (recommended
   for v1), or a request to the sender through the transfer worker.
7. **Digest in references:** carried (recommended) or left out.
8. **Scope:** adding from links and files on the desktop only in v1.
9. **Terms:** at first run and, on existing installs, at first sticker use
   (recommended); binding language; three years' record retention.
10. **Server defaults:** 1 GiB and 100 packs per user, 20 GiB per server, on;
    nuschl.eu's servers review in triage.nuschl.eu (#205)?
11. **#274 alignment:** sticker images as attachment reports, packs opened
    with the envelope's pack key.
