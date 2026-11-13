# Sticker packs: a client library, hosted where they are used

Planning document, 2026-11-13, stacked on the emoji picker (#250, `80ed9812`).
Nothing here is implemented. Part of #253; it grants no approval.

Owner direction (2026-11-13), in short: clients keep their own sticker
library and upload a pack to a server on first use there; no central host.
The desktop importer for external pack links follows the clean-room sticker
interface spec (kept outside the repository), names no external service,
and checks for an earlier import before any external request.

## 1. Pack format and crypto

Objects `0..=N+1`: `0` the manifest, `1..=N` the stickers (`1 <= N <= 200`),
`N+1` an optional cover.

```proto
message PackManifest {
  uint32 version = 1;             // 1
  string title = 2;               // <= 64 graphemes
  string author = 3;              // <= 64 graphemes
  uint32 cover = 4;               // object index of the cover
  repeated Sticker stickers = 5;  // display order; entry i is object i + 1
  repeated bytes objects = 6;     // h_1 .. h_{N+1}
  bytes import_tag = 7;           // §5.2, imported packs only
}
message Sticker {
  string emoji = 1;   // required: one emoji of #250's table
  string alt = 2;     // optional, <= 120 bytes
  uint32 kind = 3;    // PNG, APNG, WebP, animated WebP (sniffed)
  uint32 width = 4; uint32 height = 5; uint32 frames = 6; uint32 ms = 7;
  bytes digest = 8;   // SHA-256 of the image bytes
}
```

Images keep their validated bytes, so the digest matches an imported file.

```
PRK    = HKDF-Extract(salt = "nuschl stickers/1", IKM = pack_key[32])
k_i    = HKDF-Expand(PRK, "nuschl stickers/1 object" || u16 i, 32)
ek_i   = HKDF-Expand(k_i, "nuschl stickers/1 aead", 32)
c_i    = SHA-256("nuschl stickers/1 commit" || u16 i || k_i)
inner  = u32 len || bytes || zeros to Padmé(4 + len)      (as files/1)
blob_i = "NST1" || nonce[12] || c_i ||
         AEAD(ek_i, nonce, inner, aad = "NST1" || u16 i || c_i)
h_i    = SHA-256(blob_i)       object id;   pack_id = h_0
```

- `pack_id` fixes the manifest, which lists each `h_i`, so receivers verify
  every blob from any source. A pack is its hash; editing makes a new pack.
- Opening derives `k_i`, compares `c_i` in constant time, then opens the
  AEAD. With the key committed and the nonce in the blob, a ciphertext opens
  to one plaintext: the commitment fix of Albertini et al. (USENIX Security
  2022) and Bellare–Hoang (EUROCRYPT 2022), as files/1's `C`.
- Per-object keys let a report reveal `k_i` and `k_0` only; stores are
  write-once. Padmé hides sizes, which would fingerprint imported packs.

| Option | Commits | Note |
| --- | --- | --- |
| AES-256-GCM alone | no | invisible salamanders (Dodis et al., CRYPTO 2018) |
| ChaCha20-Poly1305 alone | no | partitioning oracles (Len et al., USENIX 2021) |
| Either + `c_i` | key and slot | 32 bytes, standard hashes |
| CTX (Chan–Rogaway, ESORICS 2022) | key, nonce, AD | needs the AEAD's internal tag |
| Message franking (Grubbs et al., CRYPTO 2017) | sender, message | MLS-layer work |

Both AEADs are in the tree in `libcrux-aead` 0.0.10, beside `libcrux-hkdf`
and `libcrux-sha2`. Its ChaCha20-Poly1305 is verified HACL\* code and
constant-time on wasm32; `libcrux-aes` needs its verification status checked
(D4). Message franking (who sent a sticker) is missing for all messages;
`StickerRef` carries `h_i`, so a future scheme covers stickers. The portable
crate `nuschl-stickers` (format, keys, manifest, links, tag; no I/O) runs in
the native driver, which handles pack keys as it does files/1 keys; the
content worker gets plaintext images only. The web client needs libcrux on
wasm32, as the screen-share plan proposes.

**A server learns** uploader, time, pack id, object hashes, Padmé sizes,
fetchers and equal import tags; colluding servers can link a pack id. Titles,
images, emoji and external pack ids stay hidden.

## 2. The library

The desktop keeps a `sticker_packs` table in the storage worker (pack id,
key, order, origin, import tag, certification; settings.json cannot gain
keys, #250) and blobs under `stickers/<hash>` in the profile; the web client
uses IndexedDB, per browser. Importing (§5) and creating from image files
run the content worker checks (§5.4), a review and the certification (§4).
Saving from a message verifies the pack and adds it as "saved". Deleting a
pack deletes local blobs; servers follow their retention.

## 3. Hosting on Nuschl servers

On a server with capability `stickers/1`, sending a sticker runs:

1. `StickerHave {pack_id, hashes}` returns the objects held or blocked.
2. `StickerPut` (write-once) uploads the manifest, with the licence record
   (§4.3), and the sent sticker; then the message with its `StickerRef`.
3. The other objects follow in the background, so recipients can save the
   pack (D3); an interrupted upload resumes on the next connection.

Receivers `StickerGet` by hash from that server and verify. Any
authenticated user may fetch; reading needs the key from a message. The
server checks each blob's hash, size and header. Blobs travel on files/1's
second TLS connection (docs/files.md), off the control and voice
connection; the web client reaches it through the web relay. The store is
the server database through the storage worker, one row per object.

| Limit | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Formats | PNG, APNG, WebP still and animated | the content worker's decoders |
| Animation | <= 3 s, <= 150 frames, >= 20 ms per frame | bounded decode |
| Manifest | <= 64 KiB encrypted | 200 entries with alt text and hashes |
| Pack | <= 64 MiB of blobs | 201 x 301 KiB padded |
| Quota | 1 GiB and 100 packs per user, 20 GiB per server, 4 uploads in flight | operator cost |

A pack lives while a retained message references it, as files/1's kept
objects do; `ChatSend.stickers` lists a message's pack ids, so E2EE servers
count references from metadata. Quota is charged to the first uploader.
`services.nuschl.stickers` in `nix/module.nix` sets `enable` (default true),
`maxPackBytes`, the user and store quotas and `importLookups` (§5.2).

## 4. Licence and certification

### 4.1 When and how

Once per pack, when it enters the library by import or creation (D2); a
saved pack carries the original grant. Each first upload sends the record.

> [ ] I made these stickers or have the right to share them, and I accept
> the [sticker licence].
>
> Servers you send them on and people who receive them can use them in
> Nuschl, also after you delete the pack.

Imports use the sentence "A link to a pack doesn't give you the right to
share it." "Add pack" waits for the tick; the link opens the full text.

### 4.2 Licence text, version 1

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
8. **What is recorded.** A server you send the pack on records your account
   there, the pack id, the time, this licence's version and hash, and, if
   you have a Nuschl identity, your device's signature over them. Its
   operator keeps this as proof of the licence and to handle reports, while
   it hosts the pack and for three years after, based on the licence
   agreement and the interest in defending legal claims (GDPR Art. 6(1)(b)
   and (f)). You can ask that operator for a copy.

Best-effort wording by a non-lawyer; worth a legal check before launch.

Sources: GitHub Terms of Service D.3–D.5, D.7; Discord Terms "Your content";
CC BY 4.0 legal code §2(a), §2(b)(1), §6(c); UrhG §§13, 14, 31, 31a, 32,
42; OLG Frankfurt 6 U 174/23 (2024); BGB §195; GDPR Art. 6, 13, 15; DSA
Art. 16, 17, 20.

### 4.3 Version, hash and record

The text is `licence/stickers-1.en.txt`, shipped in the client, with id
`nuschl-sticker-licence/1` and hash SHA-256 of its canonical bytes (UTF-8,
NFC, LF line ends, no trailing spaces). The first `StickerPut` of a pack
carries id, hash, confirmation time, origin (certified or saved) and, with a
Nuschl identity, a device-key signature over `"nuschl stickers/1 licence"
|| pack_id || id || hash || time`, made by the crypto worker as a typed
request. The server accepts known hashes and adds its account id and time.

## 5. Importer for external pack links

### 5.1 Flow

1. "Add pack" takes a pasted link in either form of the spec (§8) and
   validates both values; the client registers no URL scheme handler.
2. It derives the tag (§5.2) and looks in the library, then asks the
   connected Nuschl servers (D6). A hit fetches and checks (D5) the pack
   from that server and adds it as saved.
3. A miss says "Downloads the pack once", fetches and checks manifest and
   images (§5.3, §5.4), shows a review (missing emoji are chosen in #250's
   picker), asks for certification (§4.1) and encrypts into the library.
   Nothing is uploaded until the pack is sent somewhere.

The help line reads "Nuschl is independent of the apps pack links come from."

### 5.2 Dedupe before any external request

```
tag    = HMAC-SHA256(ext_pack_key,
                     "nuschl stickers/1 import tag" || ext_pack_id)
wrap   = HKDF(IKM = ext_pack_key, salt = "nuschl stickers/1 import",
              info = "wrap" || ext_pack_id, 32)
record = AEAD(wrap, {pack_id, pack_key, ext manifest and image SHA-256s})
```

A first upload of an imported pack stores `{tag, record}`, and
`StickerImportLookup {tag}` returns up to four records. A server learns equal
imports and their times; an operator holding a link learns who imported it.

A first importer can poison a tag with altered images, and only the external
CDN can prove a pack's contents. The recommended check (D5) fetches the
external manifest and two random images (3 requests of up to 202) and
compares hashes with the record; on a mismatch the client reports the record
(§6) and imports a fresh copy.

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
- Ciphertext is cached by tag and index, so a resume refetches nothing,
  until the pack is in the library or for 7 days.

### 5.4 Untrusted manifest and images

The external manifest is parsed in `nuschl-stickers` after its MAC check,
within 10 KiB: invalid UTF-8 becomes U+FFFD, Cc and Cf characters go, and
duplicate or out-of-range indexes reject the pack. Images go to the content
worker one at a time through new typed requests, `StickerCheck` (kind, size,
frames, duration, a 96 px preview) and `StickerFrames` (frames at display
size). The same checks run for created packs and every received blob.

| Bound | Value |
| --- | --- |
| Input | <= 300 KiB; PNG, APNG or WebP by magic bytes; declared MIME ignored |
| Header dimensions | <= 512 x 512, checked before decoding |
| Frames | <= 150 and <= 3 s, decoded one at a time, two resident |
| Time | 2 s wall per image, under the worker's memory limit |
| Reply | frames at <= 160 px, under the 12 MiB IPC frame |
| Desktop cache | 64 MiB of decoded frames, least recently used first |

Results are bound to their import or message, request and generation.

**External hosts:** the importer must recognise the external link host and
contact the external CDN host (spec §2, §8). Their names appear only as two
constants in the `external_pack` module and its tests, with a build and runtime
switch that turns the importer off (D7). Identifiers, files and UI use neutral
names.

## 6. Moderation, per server

- **Report:** "Report sticker" sends `StickerReport` to the server the
  message arrived on: pack id, index, `k_i`, `k_0`, a reason and up to 1,000
  characters; "Include the whole pack" adds the pack key.
- **Evidence:** the server recomputes `c_i` from `k_i` and finds `h_i` in its
  manifest: this key opens that object, first uploaded by account X at time
  T under licence hash H.
- **Review:** moderators see, in the client's server administration, the
  reported stickers (decoded in their content worker), the manifest, the
  pack if included, the uploader's account, licence record and earlier
  reports; other packs' contents and E2EE usage stay hidden.
- **Removal:** the server deletes and blocks the pack id and hashes; its
  clients show "Sticker unavailable on this server", other servers are
  unaffected, and anyone can hide a pack locally. Uploaders get a statement
  of reasons and one appeal (DSA Art. 17, 20); three upheld removals in a
  year suspend uploads there.
- **nuschl.eu's servers** are operators like any other, possibly reviewing
  in triage.nuschl.eu (#205) (D11).

## 7. Sending and showing stickers

### 7.1 The reference

```proto
message StickerRef {
  bytes pack_id = 1; bytes pack_key = 2;  // h_0 and the key, 32 bytes each
  uint32 index = 3;  bytes object = 4;    // 1..=200 and h_i
  string emoji = 5;  string alt = 6;      // one emoji of the table; <= 120 B
}
```

About 190 bytes; a sticker message has no text or attachments. Receivers
validate every field as the spec's §4.3 does and show a sticker once its
manifest and blob verify.

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Mumble |
| --- | --- | --- | --- |
| Blobs | the server's store | the same, ciphertext | none |
| Carrier | `sticker` field on `ChatSend` and `Message` | `sticker` in kind 13's `ChatBody` (#216), group feature `stickers/1` | `nuschl/sticker/1` over message 26 (#93) |
| Legacy text | the sender's inline image, for stock clients | none | the visible `TextMessage` |
| Pack key known to | the server | group members | the server and listed sessions (D8) |

On stock servers the visible message is the first frame as an `<img>` from
the content worker's `EncodeImage`, within the server's limit, then the
emoji, linked to the annotation by `msg_id` and text digest (#216 §2.4).
"Save pack" asks the online sender's client over message 26, which sends the
pack through the magic-wormhole transfer worker; the receiver verifies it
by pack id (D9).

### 7.2 Rendering and accessibility

The driver decrypts and the content worker decodes (the content WASM worker
on the web); rows show stickers at 160 logical px. Animation plays while
visible, holds its first frame under reduced motion and plays on hover or
focus. Unavailable packs show the emoji and "Sticker unavailable on this
server". The accessible name is "Sticker: <alt text, or the emoji's name>",
with the pack title as description; copying copies the emoji.

## 8. The picker (#250)

The Stickers tab replaces "Stickers are coming soon." with the library: pack
tabs like the emoji category row (Recent, the packs, "+" Add pack: link or
files), cells keyed by grid position with §7.2's names and arrow keys, and
search over each sticker's emoji names, keywords and alt text. Click sends
and closes, Shift+click sends and stays open; a first send on a server shows
"Uploading pack". Empty state: "No sticker packs yet" and "Add pack". A
received sticker's menu has "Save pack" and "Report sticker". The web picker
matches; adding from links and files is desktop only (D12).

## 9. Slices

| # | Slice | Needs | Size |
| --- | --- | --- | --- |
| S1 | `nuschl-stickers`: format, keys, manifest, links, tag, vectors | — | M |
| S2 | Content worker `StickerCheck` and `StickerFrames`, bounds | — | M |
| S3 | Desktop library, picker tab, local rendering, create from files | S1, S2 | M |
| S4 | Importer: dedupe, fetching, review, certification | S3 | L |
| S5 | files/1's second TLS connection (docs/files.md) | — | L |
| S6 | nuschld store: Have/Put/Get, limits, retention, records, lookups, NixOS | S1, S5 | L |
| S7 | Sending and receiving on the three paths | S3, S6, #93, #216 | L |
| S8 | Reports, removal, blocks, appeals | S6 | M |
| S9 | Stock "Save pack" through the transfer worker; web library and picker | S7 | M |

## 10. Tests

- Vectors for keys, commitments, hashes and blobs; a blob in another slot, a
  wrong manifest hash and a GCM ciphertext built for two keys are refused;
  the content worker refuses a declared 65535 x 65535 PNG, a 10,000-frame
  APNG, long WebP, truncated files and MIME mismatches.
- A synthetic external pack from a test encoder on a local CDN fixture in a
  VM (tests contact local fixtures only): a library hit makes 0 requests, a
  server hit 0 or 3, a miss one per file, a resume none; `Retry-After` holds.
- Two nuschld VMs: a send on A uploads to A only; after saving, B gets one
  upload and a second send none; quota and hash failures are refused;
  removal on A leaves B serving; E2EE holds ciphertext only; expired
  references delete blobs.
- Desktop smoke in headless GNOME Shell with AT-SPI names; stock Mumble in a
  VM shows the inline image, Nuschl shows one sticker, and "Save pack"
  verifies the transferred pack.

## 11. Open questions (the spec does not say)

1. Does the external CDN use a publicly trusted certificate, or the private
   CA the spec mentions (then pinned for that host only)?
2. Its rate limits, `Retry-After` use and expected request headers.
3. Whether its terms allow third-party clients to fetch packs.

## 12. Decisions for the owner

1. **Hosting:** per server on first use (recommended), or a central host
   (`stickers.nuschl.eu` / `art.nuschl.eu`): one moderation point, one
   licensee, references valid everywhere and CDN caching; its operator
   carries every pack's legal duties, sees every fetching IP and is a single
   point of failure.
2. **Certification:** once per pack at library entry (recommended), or once
   per server at first upload, which names each operator and asks more.
3. **First upload:** the whole pack in the background (recommended) or only
   the stickers sent.
4. **AEAD:** ChaCha20-Poly1305 (recommended) or AES-256-GCM, both libcrux.
5. **Dedupe hit:** verify with 3 external requests (recommended) or none.
6. **Lookup scope:** all connected Nuschl servers (recommended) or one.
7. **External hosts:** two constants plus an off switch (recommended), or a
   configuration file.
8. **Pack key on stock and non-E2EE servers:** send it (recommended) or the
   sticker key only.
9. **Stock "Save pack"** from the sender's client (recommended) or none.
10. **Server defaults:** 1 GiB and 100 packs per user, 20 GiB per server, on.
11. **nuschl.eu's servers:** accept stickers? Review in triage.nuschl.eu?
12. **Scope:** adding from links and files on the desktop only in v1.
13. **Licence:** binding language of translations; three years' retention.
14. **Shared blocklists** between operators: later, or per operator only.
