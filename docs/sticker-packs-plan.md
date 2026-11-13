# Sticker packs: global dedupe locally, conversation copies remotely

Planning document, 2026-11-13, stacked on the emoji picker (#250, `80ed9812`).
Nothing here is implemented. Part of #253; it grants no approval.

Owner direction (2026-11-13): clients keep their own library, one local copy
per sticker, and send stickers as conversation-scoped encrypted objects, as
the clean-room sticker interface spec (kept outside the repository) does in
its §9. The desktop importer for external pack links follows that spec.

## 1. The model

| Where | What | Keyed by |
| --- | --- | --- |
| Client | one copy of each sticker image and its decoded frames, whatever conversation or server it came from or went to | SHA-256 of the image bytes (`digest`) |
| Client | the library: packs the user imported, created or saved | `pack_id` (§2) |
| Server | a files/1 kept object per sticker per sender and conversation | the object's random key and root |
| Server | pack objects shared on request in one conversation (§5) | the same |

- **Sending** uploads the sticker as an ordinary files/1 kept object
  (docs/files.md): fresh random key, Padmé padding, ChaCha20-Poly1305 STREAM,
  BEP 52 root in the message. Sending it again in the same conversation
  references the sender's existing object, as files.md §4.3 allows an owner;
  another conversation gets a new object under a new key.
- **Commitment:** files/1's root `R` binds key, length and ciphertext, so an
  object opens to one image (key commitment as in Albertini et al., USENIX
  Security 2022), and message franking (#274) binds the sender. Stickers
  need no crypto of their own.
- **A server learns** sizes, timing and which conversation an object belongs
  to. In E2EE conversations it cannot link one sticker across conversations
  or tell a sticker from another image. A `history/1` server reads keys and
  digests, as it reads the rest of those messages.

## 2. The library

```proto
message PackManifest {
  uint32 version = 1;             // 1
  string title = 2;               // <= 64 graphemes
  string author = 3;              // <= 64 graphemes
  uint32 cover = 4;               // index into stickers
  repeated Sticker stickers = 5;  // 1..200, display order
  bytes import_tag = 6;           // §6.2, imported packs only
}
message Sticker {
  string emoji = 1;   // required: one emoji of #250's table
  string alt = 2;     // optional, <= 120 bytes
  uint32 kind = 3;    // PNG, APNG, WebP, animated WebP (sniffed)
  uint32 width = 4; uint32 height = 5; uint32 frames = 6; uint32 ms = 7;
  bytes digest = 8;   // SHA-256 of the image, kept byte for byte
}
```

`pack_id = SHA-256("nuschl stickers/1 pack" || manifest bytes)`; with the
digests inside, it fixes every image. The desktop keeps a `sticker_packs`
table in the storage worker (settings.json cannot gain keys, #250) and images
under `stickers/<digest>` in the profile; the web client uses IndexedDB.
Received stickers outside the library share that store, bounded to 256 MiB,
least recently used first. Imports (§6) and packs created from image files
pass the content worker checks (§6.4), a review and the certification (§4).

## 3. Sending and showing stickers

### 3.1 The reference

```proto
message StickerInfo {
  bytes digest = 1;                       // the local cache key
  string emoji = 2;  string alt = 3;      // one emoji of the table; <= 120 B
  bytes pack_id = 4; uint32 index = 5;    // for View pack and Save pack
}
```

A sticker message is one `FileRef` (files.md §4.2) plus `StickerInfo`, with
no text. Receivers validate every field as the spec's §4.3 does.

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Mumble |
| --- | --- | --- | --- |
| Image | files/1 kept object in the conversation | the same | the inline legacy image |
| Carrier | `ChatSend` attachment plus a `sticker` field | `FileRef` and `sticker` in kind 13's `ChatBody` (#216), group feature `stickers/1` | `nuschl/sticker/1` over message 26 (#93) |
| Legacy text | the sender's inline image, for stock clients | none | the visible `TextMessage` |
| Server reads | keys and digests | object ids and sizes | the annotation |

The first upload of a sticker in a conversation carries the licence record
(§4.2). On stock servers the visible message is the sticker's first frame as
an `<img>` from the content worker's `EncodeImage`, within the server's
limit, then the emoji, linked to the annotation by `msg_id` and text digest
(#216 §2.4).

### 3.2 Receiving and rendering

1. If `digest` is in the local store, the client shows that copy and
   downloads nothing.
2. Otherwise it downloads the object, checks `R`, decrypts, checks the
   image's SHA-256 against `digest`, and stores it. The content worker
   decodes it (the content WASM worker on the web).
3. On stock servers a cached `digest` shows the original, animation
   included; otherwise the inline still.

Rows show stickers at 160 logical px; animation plays while visible, holds
its first frame under reduced motion and plays on hover or focus. A removed
object shows the emoji and "Sticker unavailable". The accessible name is
"Sticker: <alt text, or the emoji's name>"; copying copies the emoji.
Results are bound to their message, request and generation.

## 4. Licence and certification

Certification happens once per pack, when it enters the library by import or
creation (D2); a saved pack carries the original grant.

> [ ] I made these stickers or have the right to share them, and I accept
> the [sticker licence].
>
> Servers you send them on and people who receive them can use them in
> Nuschl, also after you delete the pack.

Imports use the sentence "A link to a pack doesn't give you the right to
share it." "Add pack" waits for the tick; the link opens the full text.

### 4.1 Licence text, version 1

1. **Your confirmation.** By adding a sticker pack to Nuschl you confirm that
   you created its images and text, or that everyone holding rights in them
   has allowed you to license them on these terms, and that the pack, used
   as this licence allows, does not infringe anyone's rights or break the
   law.
2. **Servers.** You grant the operator of every Nuschl server the pack is
   sent on, by you or by people who received it, a simple (non-exclusive),
   worldwide, royalty-free, perpetual and irrevocable right to host, store,
   reproduce, distribute, publicly display and make available the pack
   through that server, including the technical changes these need: format
   conversion, resizing, compression, padding and encryption.
3. **Recipients.** You grant everyone who receives the pack or its stickers
   the same kind of right to use, display, save and send them on within
   Nuschl. Each operator may pass these rights on to its users.
4. **Irrevocable.** You can delete a pack from your library and ask an
   operator to stop hosting it. Copies people received, and their rights,
   stay.
5. **Removal.** Each operator decides what its server hosts and may refuse
   or remove any pack at any time, for example after a report or a legal
   notice.
6. **Other sources.** Imported packs come from third-party sources. Nuschl
   is independent of them, and they do not endorse Nuschl or its importer.
7. **Your moral rights.** You remain the author. EU and German law keep some
   rights with you: to be named as the author, to object to distortion of
   your work, and to recall rights if a work stops reflecting your
   convictions. You agree that stickers appear in messages with no author
   line, and that the pack shows the author you entered.
8. **What is recorded.** A server you send stickers on records your account
   there, the stickers uploaded, the time, this licence's version and hash,
   and, if you have a Nuschl identity, your device's signature over them. Its
   operator keeps this as proof of the licence and to handle reports, while
   it hosts the pack and for three years after, based on the licence
   agreement and the interest in defending legal claims (GDPR Art. 6(1)(b)
   and (f)). You can ask that operator for a copy.

Best-effort wording by a non-lawyer; worth a legal check before launch.

Sources: GitHub ToS D.3–D.5, D.7; Discord Terms "Your content"; CC BY 4.0
§2(a), §2(b)(1), §6(c); UrhG §§13, 14, 31, 31a, 32, 42; OLG Frankfurt 6 U
174/23 (2024); BGB §195; GDPR Art. 6, 13, 15; DSA Art. 16, 17, 20.

### 4.2 Version, hash and record

The text is `licence/stickers-1.en.txt`, shipped in the client, with id
`nuschl-sticker-licence/1` and hash SHA-256 of its canonical bytes (UTF-8, NFC,
LF, no trailing spaces). A sticker object's first upload carries id, hash,
time, origin (certified or saved) and, with a Nuschl identity, a crypto-worker
signature over `"nuschl stickers/1 licence" || R || id || hash || time`. The
record names the object, so pack ids stay off the server. The server accepts
known hashes and adds its account id and time.

## 5. View pack and Save pack

1. "View pack" or "Save pack" sends `PackRequest {pack_id}` to the
   conversation: a typed message, an MLS message in E2EE, message 26 on
   stock servers.
2. A client holding the pack (the sender, or anyone who saved it) answers
   for shared packs (D8) with a pack message: `FileRef`s for the manifest
   and every image, as kept objects in that conversation, reusing its own
   objects already there. On stock servers, and for requesters with no
   Nuschl identity, the pack goes through the magic-wormhole transfer
   worker.
3. The requester checks `pack_id` and every digest, then shows the pack or
   adds it as saved.

Pack objects are per conversation too (D3): one storage path, and the server
cannot link a pack across conversations. A per-server content-addressed pack
store would store each pack once per server and expose its id to the server.

## 6. Importer for external pack links

### 6.1 Flow

1. "Add pack" takes a pasted link in either form of the spec (§8) and
   validates both values; the client registers no URL scheme handler. The
   help line reads "Nuschl is independent of the apps pack links come from."
2. It derives the tag (§6.2) and looks in the library, then asks the
   connected Nuschl servers (D5). A hit downloads and checks (D4) the
   server's import copy and adds the pack as saved.
3. A miss says "Downloads the pack once", fetches and checks manifest and
   images (§6.3, §6.4), shows a review (missing emoji are chosen in #250's
   picker), asks for certification (§4) and adds the pack to the library.

### 6.2 Dedupe before any external request

```
tag    = HMAC-SHA256(ext_pack_key,
                     "nuschl stickers/1 import tag" || ext_pack_id)
wrap   = HKDF(IKM = ext_pack_key, salt = "nuschl stickers/1 import",
              info = "wrap" || ext_pack_id, 32)
record = AEAD(wrap, {FileRef of the import copy, ext manifest and image
                     SHA-256s})
```

The import copy is the pack as one files/1 kept object, scoped to the server
and fetchable by anyone presenting its tag. A client uploads it in the
background the first time it sends a sticker of that pack on a server (D5),
with `{tag, record}`; `StickerImportLookup {tag}` returns up to four records.
A server learns equal imports and their times; an operator holding a link
learns who imported it.

A first importer can poison a tag, and only the external CDN can prove a
pack's contents. The recommended check (D4) fetches the external manifest and
two random images (3 requests of up to 202) and compares hashes; a mismatch
is reported (§7) and imported afresh.

### 6.3 Polite fetching

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

### 6.4 Untrusted manifest and images

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
off (D6); identifiers, files and UI use neutral names.

## 7. Moderation

- **Report:** a sticker report is an attachment report under message
  franking (#274): it reveals that object's key and the franked envelope,
  so the moderator sees the image, `StickerInfo`, the sender and the time.
  `R` proves that the key opens that object. #274 still routes sticker
  reports to a central service; that row becomes this attachment report.
- **Review:** the conversation's moderators see only reported objects.
- **Removal:** moderator deletion tombstones the message and releases its
  objects (files.md §1.5); clients show "Sticker unavailable".
- **Limits of unlinkability:** in E2EE conversations a server cannot
  recognise a removed sticker elsewhere, so each copy needs its own report.
  A `history/1` server can keep a list of removed digests and refuse them.
- **Uploaders** get reasons and one appeal (DSA Art. 17, 20); three upheld
  removals in a year suspend uploads there. Anyone can hide a pack locally.
- Import copies are reported and removed like other objects; a removed
  import tag stays blocked.

## 8. Storage and limits

Per-conversation copies cost a sticker once per sender and conversation,
where a per-server store costs it once per server. A user sending 50
distinct stickers of 60 KiB in 20 conversations stores about 60 MiB (50 x
20 x 60 KiB) where a per-server store would hold 3 MiB. A pack shared in a
conversation costs up to 64 MiB there. Retention follows the referencing
messages (files.md §1.5).

| Limit | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Formats | PNG, APNG, WebP still and animated | the content worker's decoders |
| Animation | <= 3 s, <= 150 frames, >= 20 ms per frame | bounded decode |
| Manifest | <= 64 KiB | 200 entries with alt text |
| Sticker objects | count toward the sender's files/1 quota | one accounting |
| Pack shares | 64 MiB each, 4 a day per user | the largest cost |
| Import copies | 64 MiB each, 2 GiB per server | one per external pack |

`services.nuschl.stickers` in `nix/module.nix` sets `enable` (default true),
`packShareBytes`, `packSharesPerDay` and `importCopies` (bytes, or off).
Sticker objects use the files/1 settings.

## 9. The picker (#250)

The Stickers tab replaces "Stickers are coming soon." with the library: pack
tabs like the emoji category row (Recent, the packs, "+" Add pack: link or
files), cells keyed by grid position with §3.2's names and arrow keys, and
search over each sticker's emoji names, keywords and alt text. Click sends and
closes, Shift+click sends and stays open. Empty state: "No sticker packs yet"
and "Add pack". A received sticker's menu has "View pack", "Save pack" and
"Report sticker". The web picker matches; adding from links and files is
desktop only (D11).

## 10. Slices

| # | Slice | Needs | Size |
| --- | --- | --- | --- |
| S1 | `nuschl-stickers`: manifest, `pack_id`, `StickerInfo`, links, tag, vectors | — | S |
| S2 | Content worker `StickerCheck` and `StickerFrames`, bounds | — | M |
| S3 | Desktop library, digest store, picker tab, create from files | S1, S2 | M |
| S4 | Importer: local dedupe, fetching, review, certification | S3 | L |
| S5 | files/1 kept objects end to end (docs/files.md §8) | — | L |
| S6 | Sending and showing stickers on the three paths, resend reuse | S3, S5, #93, #216 | M |
| S7 | View and Save pack: requests, pack messages, transfer worker | S6 | M |
| S8 | Import copies and lookups on nuschld; NixOS option | S4, S5 | M |
| S9 | Sticker reports through #274; removed digests on `history/1` | S6, #274 | S |
| S10 | Web: library, picker, rendering, sending | S1, S6 | M |

## 11. Tests

- Vectors for `pack_id`, tags and records. The content worker refuses a
  declared 65535 x 65535 PNG, a 10,000-frame APNG, long WebP, truncated
  files and MIME mismatches.
- A synthetic external pack on a local CDN fixture in a VM: a library hit
  makes 0 requests, a server hit 0 or 3, a miss one per file, a resume none.
- Two conversations on one nuschld VM: the same sticker yields two objects
  with different keys and roots; a resend in one conversation adds no
  object; a receiver holding the digest downloads nothing; a wrong digest
  or root is refused; Save pack creates objects in that conversation only.
- A sticker report verifies through #274 and deletion releases the object.
- Desktop smoke with AT-SPI names; stock Mumble shows the inline image,
  Nuschl shows one sticker, and Save pack verifies the transferred pack.

## 12. Open questions (the spec does not say)

1. Does the external CDN use a publicly trusted certificate, or the private
   CA the spec mentions (then pinned for that host only)?
2. Its rate limits, `Retry-After` use and expected request headers.
3. Whether its terms allow third-party clients to fetch packs.

## 13. Decisions for the owner

1. **Hosting:** per-conversation files/1 objects (recommended; unlinkable in
   E2EE, one storage path), a per-server content-addressed store (about 20x
   less storage in the example of §8; links stickers across conversations),
   or a central host (`stickers.nuschl.eu` / `art.nuschl.eu`).
2. **Certification:** once per pack at library entry (recommended), or once
   per server at first upload.
3. **Pack shares:** per conversation on request (recommended), or a
   per-server pack store.
4. **Dedupe hit:** verify with 3 external requests (recommended) or none.
5. **Import copies:** on servers where a pack's stickers are sent
   (recommended), or local dedupe only.
6. **External hosts:** two constants plus an off switch (recommended), or a
   configuration file.
7. **Digest in references:** carried, so receivers skip downloads
   (recommended), or left out, so every receipt downloads.
8. **Pack requests:** any holder answers for shared packs, all shared by
   default (recommended), or the sender only.
9. **Server defaults:** pack shares 64 MiB and 4 a day per user, import
   copies 2 GiB per server, on by default.
10. **nuschl.eu's servers** are operators like any other: accept stickers,
    and review in triage.nuschl.eu (#205)?
11. **Scope:** adding from links and files on the desktop only in v1.
12. **Licence:** binding language of translations; three years' retention.
13. **#274 alignment:** sticker reports as attachment reports, with that
    plan's sticker row updated to match.
