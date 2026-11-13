# Sticker packs: a client library, hosted where they are used

Planning document, 2026-11-13, stacked on the emoji picker (#250, `80ed9812`).
Nothing here is implemented. Part of #253; it grants no approval.

Owner direction (2026-11-13): clients keep their own library and upload a pack
to a server on first use there. The desktop importer follows the clean-room
sticker interface spec (kept outside the repository).

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

- `pack_id` fixes the public index of object hashes, so receivers verify
  every blob from any source and servers check membership with no key. A
  pack is its hash; editing makes a new pack.
- Opening derives `ek_i` from `k_i`, compares `c_i` in constant time, then
  opens the AEAD. With the key committed and the nonce in the blob, a
  ciphertext opens to one plaintext: the commitment fix of Albertini et al.
  (USENIX Security 2022) and Bellare–Hoang (EUROCRYPT 2022), as files/1's `C`.
- A sticker message carries only that sticker's `k_i`, which reveals nothing
  about the other stickers or the manifest. Stores are write-once. Padmé
  hides sizes, which would fingerprint imported packs.

| Option | Commits | Note |
| --- | --- | --- |
| AES-256-GCM alone | no | invisible salamanders (Dodis et al., CRYPTO 2018) |
| ChaCha20-Poly1305 alone | no | partitioning oracles (Len et al., USENIX 2021) |
| Either + `c_i` | key and slot | 32 bytes, standard hashes |
| CTX (Chan–Rogaway, ESORICS 2022) | key, nonce, AD | needs the AEAD's internal tag |
| Message franking (Grubbs et al., CRYPTO 2017) | sender, message | MLS-layer work |

Both AEADs are in `libcrux-aead` 0.0.10, beside `libcrux-hkdf` and
`libcrux-sha2`; its ChaCha20-Poly1305 is verified HACL\* code, constant-time on
wasm32, and `libcrux-aes` needs its verification checked (D4). Message franking
(who sent a sticker) is missing for all messages; `StickerRef` carries `h_i` and
`c_i` for a future scheme. The portable crate `nuschl-stickers` (format, keys,
manifest, links, tag; no I/O) runs in the native driver; the content worker gets
plaintext images only, and the web client needs libcrux on wasm32, as the
screen-share plan proposes.

**A server learns** uploader, time, pack id, object hashes, Padmé sizes,
fetchers and equal import tags; colluding servers can link a pack id. Titles,
images, emoji and external pack ids stay hidden.

## 2. The library

The desktop keeps a `sticker_packs` table in the storage worker (settings.json
cannot gain keys, #250) and blobs under `stickers/<hash>`; the web client uses
IndexedDB. Imports (§5) and packs created from image files pass the content
worker checks (§5.4), a review and the certification (§4). Saved packs are
verified first. Deleting a pack deletes local blobs.

## 3. Hosting on Nuschl servers

On a server with capability `stickers/1`, sending a sticker runs:

1. `StickerHave {pack_id}` returns the objects held or blocked.
2. `StickerPut` (write-once) uploads the sent sticker's blob, plus the index
   and licence record (§4.2) on the pack's first upload; the message follows.
3. The rest of the pack uploads on a save request (§7.3, D3).

Receivers `StickerGet` that one blob by hash; any authenticated user may fetch,
and reading needs the key from a message. The server checks each blob's hash
against the index, its size and header. Blobs travel on files/1's second TLS
connection (docs/files.md), reached from the web through the web relay, and are
stored in the server database, one row per object.

| Limit | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Formats | PNG, APNG, WebP still and animated | the content worker's decoders |
| Animation | <= 3 s, <= 150 frames, >= 20 ms per frame | bounded decode |
| Manifest | <= 64 KiB encrypted | 200 entries with alt text |
| Pack | <= 64 MiB of blobs | 201 x 301 KiB padded |
| Quota | 1 GiB and 100 packs per user, 20 GiB per server, 4 uploads in flight | operator cost |

A pack lives while a retained message references it, as files/1's kept objects
do; `ChatSend.stickers` lists a message's pack ids for E2EE servers. Quota is
charged to the first uploader. `services.nuschl.stickers` in `nix/module.nix`
sets `enable` (default true), `maxPackBytes`, the quotas and `importLookups`
(§5.2).

## 4. Licence and certification

Certification happens once per pack, when it enters the library by import or
creation (D2); a saved pack carries the original grant. Each first upload sends
the record.

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
8. **What is recorded.** A server you send the pack on records your account
   there, the pack id, the time, this licence's version and hash, and, if
   you have a Nuschl identity, your device's signature over them. Its
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
LF, no trailing spaces). A pack's first `StickerPut` carries id, hash, time,
origin (certified or saved) and, with a Nuschl identity, a crypto-worker
signature over `"nuschl stickers/1 licence" || pack_id || id || hash || time`.
The server accepts known hashes and adds its account id and time.

## 5. Importer for external pack links

### 5.1 Flow

1. "Add pack" takes a pasted link in either form of the spec (§8) and
   validates both values; the client registers no URL scheme handler. The
   help line reads "Nuschl is independent of the apps pack links come from."
2. It derives the tag (§5.2) and looks in the library, then asks the
   connected Nuschl servers (D6). A hit gets and checks (D5) the pack from
   that server, requesting missing objects as Save pack does (§7.3).
3. A miss says "Downloads the pack once", fetches and checks manifest and
   images (§5.3, §5.4), shows a review (missing emoji are chosen in #250's
   picker), asks for certification (§4) and encrypts into the library.
   Nothing is uploaded until the pack is sent somewhere.

### 5.2 Dedupe before any external request

```
tag    = HMAC-SHA256(ext_pack_key,
                     "nuschl stickers/1 import tag" || ext_pack_id)
wrap   = HKDF(IKM = ext_pack_key, salt = "nuschl stickers/1 import",
              info = "wrap" || ext_pack_id, 32)
record = AEAD(wrap, {pack_id, pack_key, ext manifest and image SHA-256s})
```

A first upload of an imported pack stores `{tag, record}`; `StickerImportLookup
{tag}` returns up to four records. A server learns equal imports and their
times; an operator holding a link learns who imported it.

A first importer can poison a tag, and only the external CDN can prove a pack's
contents. The recommended check (D5) fetches the external manifest and two
random images (3 requests of up to 202) and compares hashes; a mismatch is
reported (§6) and imported afresh.

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

The external manifest is parsed in `nuschl-stickers` after its MAC check, within
10 KiB: invalid UTF-8 becomes U+FFFD, Cc and Cf characters go, and duplicate or
out-of-range indexes reject the pack. Images go to the content worker one at a
time through new typed requests, `StickerCheck` (kind, size, frames, duration, a
96 px preview) and `StickerFrames` (frames at display size), for imports,
created packs and every received blob. Results are bound to their import or
message, request and generation.

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
off (D7); identifiers, files and UI use neutral names.

## 6. Moderation, per server

- **Report:** "Report sticker" sends `StickerReport` to the server the
  message arrived on: pack id, index, `h_i`, `k_i`, a reason and up to 1,000
  characters; a reporter who saved the pack may add its key.
- **Evidence:** the server recomputes `c_i` from `k_i` and the index, checks
  the blob header, and finds `h_i` in the pack's stored index: this key
  opens that object, first uploaded by account X at time T under licence
  hash H.
- **Review:** moderators see, in the client's server administration, the
  reported sticker (decoded in their content worker), the whole pack if its
  key was added, the uploader's account, licence record and earlier reports.
  Other packs' contents and E2EE usage stay hidden.
- **Removal:** the server deletes and blocks the pack id and hashes, and its
  clients show "Sticker unavailable on this server". Uploaders get reasons
  and one appeal (DSA Art. 17, 20); three upheld removals in a year suspend
  uploads there. Anyone can hide a pack locally.

## 7. Sending and showing stickers

### 7.1 The reference

```proto
message StickerRef {
  bytes object = 1;  bytes key = 2;       // h_i and k_i, 32 bytes each
  bytes commit = 3;  uint32 index = 4;    // c_i and the i it binds
  string emoji = 5;  string alt = 6;      // one emoji of the table; <= 120 B
  bytes pack_id = 7;                      // for View pack and Save pack
}
```

About 180 bytes; a sticker message has no text or attachments. Its source is
the server the message arrived on: a reference names no host, so a sender
cannot point receivers at other hosts (spec §6.6). Receivers validate every
field as the spec's §4.3 does.

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Mumble |
| --- | --- | --- | --- |
| Blobs | the server's store | the same, ciphertext | none |
| Carrier | `sticker` field on `ChatSend` and `Message` | `sticker` in kind 13's `ChatBody` (#216), group feature `stickers/1` | `nuschl/sticker/1` over message 26 (#93) |
| Legacy text | the sender's inline image, for stock clients | none | the visible `TextMessage` |
| Sticker key known to | the server | group members | the server and listed sessions |

On stock servers the visible message is the sticker's first frame as an
`<img>` from the content worker's `EncodeImage`, within the server's limit,
then the emoji, linked to the annotation by `msg_id` and text digest (#216
§2.4). Nuschl receivers show that still; Save pack brings the blobs.

### 7.2 Rendering and accessibility

A received sticker needs one blob: the driver fetches `h_i`, checks its hash and
`c_i`, decrypts with `k_i`, and the content worker (on the web, the content WASM
worker) decodes it, with no manifest or other sticker. Rows show it at 160
logical px; animation plays while visible, holds its first frame under reduced
motion and plays on hover or focus. A refused or removed blob shows the emoji
and "Sticker unavailable on this server". The accessible name is "Sticker: <alt
text, or the emoji's name>"; copying copies the emoji.

### 7.3 View pack and Save pack

The pack key and manifest move only when a recipient asks (D8):

1. "View pack" or "Save pack" sends `PackRequest {pack_id}` to the
   conversation: a typed message, an MLS message in E2EE, message 26 on
   stock servers.
2. A client holding the pack (the sender, or anyone who saved it) answers
   for shared packs (D9): on Nuschl servers it uploads missing objects and
   sends the key inside MLS or sealed with HPKE (X-Wing, libcrux) to the
   requester's device key; otherwise pack and key go through the
   magic-wormhole transfer worker.
3. The requester checks the index against `pack_id` and every blob, then
   shows the pack or adds it as saved.

## 8. The picker (#250)

The Stickers tab replaces "Stickers are coming soon." with the library: pack
tabs like the emoji category row (Recent, the packs, "+" Add pack: link or
files), cells keyed by grid position with §7.2's names and arrow keys, and
search over each sticker's emoji names, keywords and alt text. Click sends and
closes, Shift+click sends and stays open. Empty state: "No sticker packs yet"
and "Add pack". A received sticker's menu has "View pack", "Save pack" and
"Report sticker". The web picker matches; adding from links and files is desktop
only (D12).

## 9. Slices

| # | Slice | Needs | Size |
| --- | --- | --- | --- |
| S1 | `nuschl-stickers`: format, keys, manifest, links, tag, vectors | — | M |
| S2 | Content worker `StickerCheck` and `StickerFrames`, bounds | — | M |
| S3 | Desktop library, picker tab, local rendering, create from files | S1, S2 | M |
| S4 | Importer: dedupe, fetching, review, certification | S3 | L |
| S5 | files/1's second TLS connection (docs/files.md) | — | L |
| S6 | nuschld store: Have/Put/Get, limits, retention, records, lookups, NixOS | S1, S5 | L |
| S7 | Sending and showing single stickers on the three paths | S3, S6, #93, #216 | L |
| S8 | Reports, removal, blocks, appeals | S6 | M |
| S9 | View and Save pack: requests, sealed keys, on-demand upload, transfer worker | S7 | M |
| S10 | Web: library, picker, rendering, sending | S1, S7 | M |

## 10. Tests

- Vectors for keys, commitments, hashes and blobs; a blob in another slot or
  outside the index, and a GCM ciphertext built for two keys, are refused.
  The content worker refuses a declared 65535 x 65535 PNG, a 10,000-frame
  APNG, long WebP, truncated files and MIME mismatches.
- A synthetic external pack on a local CDN fixture in a VM: a library hit
  makes 0 requests, a server hit 0 or 3, a miss one per file, a resume none.
- Two nuschld VMs: a send on A uploads one blob, which a receiver fetches
  alone; Save pack uploads the rest and delivers the key sealed; B gets one
  upload after saving; quota and hash failures are refused; removal on A
  leaves B serving.
- Desktop smoke with AT-SPI names; stock Mumble shows the inline image,
  Nuschl one sticker, and Save pack verifies the transferred pack.

## 11. Open questions (the spec does not say)

1. Does the external CDN use a publicly trusted certificate, or the private
   CA the spec mentions (then pinned for that host only)?
2. Its rate limits, `Retry-After` use and expected request headers.
3. Whether its terms allow third-party clients to fetch packs.

## 12. Decisions for the owner

1. **Hosting:** per server on first use (recommended), or a central host
   (`stickers.nuschl.eu` / `art.nuschl.eu`) with one moderation point and
   CDN caching, whose operator carries every pack's legal duties and sees
   every fetching IP.
2. **Certification:** once per pack at library entry (recommended), or once
   per server at first upload.
3. **Rest of the pack:** uploaded on a save request (recommended; a sticker
   needs only its blob), or in the background, so saves work offline.
4. **AEAD:** ChaCha20-Poly1305 (recommended) or AES-256-GCM, both libcrux.
5. **Dedupe hit:** verify with 3 external requests (recommended) or none.
6. **Lookup scope:** all connected Nuschl servers (recommended) or one.
7. **External hosts:** two constants plus an off switch (recommended), or a
   configuration file.
8. **Pack key:** only on request (recommended; one sticker reveals one
   sticker), or also in E2EE messages, so members save with the sender away.
9. **Pack requests:** any holder answers for shared packs, all shared by
   default (recommended), or the sender only.
10. **Server defaults:** 1 GiB and 100 packs per user, 20 GiB per server, on.
11. **nuschl.eu's servers** are operators like any other: accept stickers,
    and review in triage.nuschl.eu (#205)?
12. **Scope:** adding from links and files on the desktop only in v1.
13. **Licence:** binding language of translations; three years' retention.
14. **Shared blocklists** between operators: later, or per operator only.
