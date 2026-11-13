# Sticker packs: a client library, hosted where they are used

Planning document, 2026-11-13, stacked on the emoji picker (#250, `80ed9812`).
Nothing here is implemented. Part of #253; it grants no approval.

Owner direction (2026-11-13), in short: users keep their own sticker library
and upload a pack to a server the first time they send from it there; there
is no central sticker host. The desktop client includes an importer for
external pack links, built from the clean-room sticker interface spec (kept
outside the repository). Our format should let moderators verify reports.
Uploaders certify their rights and grant a perpetual licence. The importer
names no external service, claims no endorsement, and checks for an earlier
import before it contacts any external host.

## 1. Summary

The client keeps a library of encrypted, content-addressed packs and uploads
a pack to a Nuschl server the first time it sends from it there; receivers
fetch from that server and can save it. Key commitment lets each server
verify reports and moderate its own copy. Stock Mumble gets an inline image
plus a typed reference over message 26. The importer checks the library and
the user's servers before fetching each external file once.

## 2. Pack format and crypto

### 2.1 Objects

A pack has objects `0..=N+1`: `0` the manifest, `1..=N` the stickers
(`1 <= N <= 200`), and an optional cover `N+1`.

```proto
message PackManifest {
  uint32 version = 1;             // 1
  string title = 2;               // <= 64 graphemes
  string author = 3;              // <= 64 graphemes
  uint32 cover = 4;               // object index of the cover
  repeated Sticker stickers = 5;  // display order; entry i is object i + 1
  repeated bytes objects = 6;     // h_1 .. h_{N+1}
  bytes import_tag = 7;           // §6.2, imported packs only
}
message Sticker {
  string emoji = 1;   // required: one emoji of #250's table
  string alt = 2;     // optional, <= 120 bytes
  uint32 kind = 3;    // PNG, APNG, WebP, animated WebP (sniffed)
  uint32 width = 4; uint32 height = 5;
  uint32 frames = 6; uint32 duration_ms = 7;
  bytes digest = 8;   // SHA-256 of the image bytes
}
```

Images keep their validated bytes: re-encoding animations needs encoders the
tree lacks, and the digest then matches the imported file (§6.2).

### 2.2 Construction

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

- **Content addressing:** `pack_id` fixes the manifest and the manifest
  lists each `h_i`, so receivers verify every blob from any source.
- **One plaintext per ciphertext:** opening derives `k_i`, compares `c_i` in
  constant time, then opens the AEAD. With the key fixed by its commitment
  and the nonce in the blob, decryption is a function. This is the
  commitment fix of Albertini et al. (USENIX Security 2022) and Bellare–Hoang
  (EUROCRYPT 2022), the pattern of files/1's `C` (docs/files.md §4.1).
- **Per-object keys:** a report reveals `k_i` and `k_0` only. Each key
  encrypts one object; the random nonce keeps an accidental re-encryption
  safe, and stores are write-once.
- **Padmé** hides exact image sizes, which would fingerprint imported packs.
- **Immutable:** a pack is its hash; editing makes a new pack and key.

| Option | Commits | Note |
| --- | --- | --- |
| AES-256-GCM alone | no | invisible salamanders (Dodis et al., CRYPTO 2018) |
| ChaCha20-Poly1305 alone | no | partitioning oracles (Len et al., USENIX 2021) |
| Either + `c_i` | key and slot | 32 bytes, standard hashes |
| CTX (Chan–Rogaway, ESORICS 2022) | key, nonce, AD | needs the AEAD's internal tag |
| Message franking (Grubbs et al., CRYPTO 2017) | sender, message | MLS-layer work |

Both AEADs are in the tree in `libcrux-aead` 0.0.10, with `libcrux-hkdf` and
`libcrux-sha2`. libcrux's ChaCha20-Poly1305 comes from verified HACL\* code;
`libcrux-aes`'s verification status needs checking before choosing
AES-256-GCM. ChaCha20-Poly1305 is constant-time on wasm32 and on CPUs
lacking AES instructions, and device linking and files/1 use it (D4).

Proving *who sent* a sticker in an MLS group is message franking, which
Nuschl lacks for all messages; `StickerRef` carries `h_i`, so a future
franking scheme covers stickers unchanged.

The portable crate `nuschl-stickers` holds format, keys, manifest, link
parsing and import tag, with no I/O. The native driver handles pack keys as
it handles files/1 keys; the content worker gets plaintext images only. The
web client needs libcrux on wasm32, as the screen-share plan proposes.

### 2.3 What a server learns

Uploader account and time, pack id, object hashes and Padmé sizes, which
users fetch which packs, and that users imported the same external pack
(equal tags). Pack ids are global, so colluding servers can see that one
pack is used on both. Titles, images, emoji and external pack ids stay
hidden; reported stickers become visible to that server's moderators.

## 3. The library

- **Desktop:** a `sticker_packs` table in the storage worker (pack id, key,
  order, origin, import tag, certification) and blobs in the profile under
  `stickers/<hash>`. settings.json cannot gain keys (#250).
- **Web:** IndexedDB, per browser; a lost library loses only the selection.
- **Entry:** importing (§6) and creating from image files run the content
  worker checks (§6.4), the review and the certification (§5). Saving from
  a message verifies the pack and adds it as "saved".
- Deleting a pack deletes the local blobs; servers follow their retention.

## 4. Hosting on Nuschl servers

### 4.1 Upload on first use

On a server with capability `stickers/1`, sending a sticker runs:

1. `StickerHave {pack_id, hashes}`: which objects the server holds or has
   blocked.
2. `StickerPut` for the manifest and the sent sticker (write-once), with the
   licence record (§5.4) on the manifest's first upload.
3. The message with its `StickerRef` (§8.1).
4. The other objects in the background, so recipients can save the whole
   pack (D3); an interrupted upload resumes on the next connection.

Receivers `StickerGet` by hash from the same server and verify. Any
authenticated user of the server may fetch; reading needs the key from a
message. The server checks each blob's hash, size and header.

Blobs travel on files/1's second TLS connection (docs/files.md), which keeps
bulk transfers off the control and voice connection; the web client reaches
it through the web relay. The store is the server database, one row per
object, through the storage worker, which gives transactional reference
counts. files/1's kept objects can later share it.

### 4.2 Limits

| What | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Formats | PNG, APNG, WebP still and animated | the content worker's decoders |
| Animation | <= 3 s, <= 150 frames, >= 20 ms per frame | bounded decode |
| Manifest | <= 64 KiB encrypted | 200 entries with alt text and hashes |
| Pack | <= 64 MiB of blobs | 201 x 301 KiB padded |
| Per user | 1 GiB and 100 packs first uploaded | operator cost |
| Per server | 20 GiB | disk |
| In flight | 4 objects per user, then backpressure | fairness |

### 4.3 Retention and NixOS

A pack lives on a server while a retained message references it, as
files/1's kept objects do. `ChatSend.stickers` lists the pack ids a message
uses, so E2EE servers count references from metadata alone. Expired packs
are deleted; a later send uploads them again. Quota is charged to the first
uploader.

`services.nuschl.stickers` in `nix/module.nix`: `enable` (default true),
`maxPackBytes`, `userQuotaBytes`, `userQuotaPacks`, `storeQuotaBytes` and
`importLookups` (answer §6.2, default true). It needs no new port, host or
outbound network.

## 5. Licence and certification

### 5.1 When and how

Once per pack, when it enters the library by import or creation (D2). A
saved pack carries the original grant, which covers sending it on. The
client keeps the confirmation and sends it with each first upload.

> [ ] I made these stickers or have the right to share them, and I accept
> the [sticker licence].
>
> Servers you send them on and people who receive them can use them in
> Nuschl, also after you delete the pack.

When importing, the sentence is: "A link to a pack doesn't give you the
right to share it." The box starts unticked, "Add pack" stays disabled until
it is ticked, and the link opens the full text in the app.

### 5.2 Licence text, version 1

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

Drafting notes:

- Point 2 names each use: German law reads a grant only as far as its
  stated purpose (§31(5) UrhG). It names no "media yet unknown", which
  needs written form (§31a). A free simple licence for everyone is allowed
  (§32(3)); this grant is a narrower case.
- Point 3 grants recipients directly, as CC BY 4.0 §2(a)(5)(A) does, which
  also covers peer transfers on stock servers. Point 4 follows CC BY 4.0
  §6(c): stopping distribution leaves the grant.
- Point 7 follows §§13, 14 and 42 UrhG: the recall right cannot be waived in
  advance, and OLG Frankfurt 6 U 174/23 (2024) read a platform clause with
  §42 intact. GitHub's D.7 waives moral rights, which German law allows
  only as consent.
- Point 8 is the GDPR Art. 13 notice; each operator is the controller of its
  records. Three years is the limitation period (§195 BGB).

Sources: GitHub Terms of Service D.3–D.5, D.7; Discord Terms "Your content";
CC BY 4.0 legal code §2(a)(1), §2(a)(4), §2(a)(5)(A), §2(b)(1), §6(c); UrhG
§§13, 14, 31, 31a, 32, 42; OLG Frankfurt 20 June 2024, 6 U 174/23; GDPR
Art. 6, 13, 15; DSA Art. 16, 17, 20.

### 5.3 Version, hash and record

The text is `licence/stickers-1.en.txt`, shipped in the client, with id
`nuschl-sticker-licence/1` and hash SHA-256 of its canonical bytes (UTF-8,
NFC, LF line ends, no trailing spaces). The client shows that exact text.
The first `StickerPut` of a pack carries id, hash, confirmation time, origin
(certified or saved) and, with a Nuschl identity, a device-key signature
over `"nuschl stickers/1 licence" || pack_id || id || hash || time`, which
the crypto worker makes as a typed request. The server accepts known hashes
and adds its account id and its own time.

## 6. Importer for external pack links

### 6.1 Flow

1. "Add pack" takes a pasted link in either form of the spec (§8) and
   validates both values. The client registers no URL scheme handler.
2. It derives the tag (§6.2), looks in the library, then asks the connected
   Nuschl servers (D6).
3. **Hit:** it fetches the pack from that server, checks it (D5) and adds it
   as saved.
4. **Miss:** "Downloads the pack once." It fetches and checks the manifest
   and images (§6.3, §6.4). The review shows title, author and stickers;
   missing or unknown emoji are chosen from #250's picker; alt text is
   optional.
5. Certification (§5.1), a fresh pack key, encryption into the library.
   Nothing is uploaded until the pack is sent somewhere.

UI strings say "pack link" and "sticker pack". The help line reads "Nuschl
is independent of the apps pack links come from."

### 6.2 Dedupe before any external request

```
tag    = HMAC-SHA256(ext_pack_key,
                     "nuschl stickers/1 import tag" || ext_pack_id)
wrap   = HKDF(IKM = ext_pack_key, salt = "nuschl stickers/1 import",
              info = "wrap" || ext_pack_id, 32)
record = AEAD(wrap, {pack_id, pack_key, ext manifest and image SHA-256s})
```

The first upload of an imported pack to a server also stores `{tag,
record}`; `StickerImportLookup {tag}` returns up to four records.

| Party | Learns |
| --- | --- |
| A server asked | that its users imported the same external pack, and when; the external id stays behind a 256-bit key |
| A link holder | our copy and its key: the audience the link already had |
| An operator holding a link | whether, when and by whom it was imported there |

A first importer can poison a tag with altered images, and only the external
CDN can prove a pack's contents. The recommended check (D5) fetches the
external manifest and two random images (3 requests of up to 202) and
compares hashes with the record. On a mismatch the client reports the
record (§7) and imports a fresh copy.

### 6.3 Polite fetching

- Only on the user's import: no background fetching, prefetch or crawling.
- HTTPS to the configured CDN host only, no cross-host redirects, a plain
  `Nuschl/<version>` User-Agent, and the user's proxy setting.
- One import at a time, two requests in flight, 100 ms between starts, each
  distinct file once (a thumbnail may share a sticker's index).
- 429 and 503 wait for `Retry-After` (at most 10 minutes); other errors back
  off 1–32 s with jitter, four attempts per file; three failed files in a
  row stop the import. 403 and 404 mean "not found".
- Received ciphertext is cached in the profile by tag and index, so a resume
  refetches nothing; the cache is deleted once the pack is in the library,
  or after seven days.
- 15 s connect and 30 s per file; reads stop at 10 KiB and 301 KiB.

### 6.4 Untrusted manifest and images

The external manifest is parsed in `nuschl-stickers` after its MAC check,
within 10 KiB. Invalid UTF-8 becomes U+FFFD; Cc and Cf characters are
removed; duplicate or out-of-range indexes reject the pack.

Images go to the content worker one at a time through new typed requests:
`StickerCheck` (kind, size, frames, duration, a 96 px preview) and
`StickerFrames` (frames at display size). The same checks run for created
packs and every received blob.

| Bound | Value |
| --- | --- |
| Input | <= 300 KiB; PNG, APNG or WebP by magic bytes; declared MIME ignored |
| Header dimensions | <= 512 x 512, checked before decoding |
| Frames | <= 150 and <= 3 s, decoded one at a time, two resident |
| Time | 2 s wall per image, under the worker's memory limit |
| Reply | frames at <= 160 px, under the 12 MiB IPC frame |
| Desktop cache | 64 MiB of decoded frames, least recently used first |

Results are bound to their import or message, request and generation.

### 6.5 Naming the external hosts

The importer must recognise the external link host and contact the external
CDN host (spec §2, §8). Their names appear only as two constants in the
`external_pack` module and its tests, with a build and runtime switch that
turns the importer off (D7). Identifiers, files and UI use neutral names.

## 7. Moderation, per server

- **Report:** "Report sticker" sends `StickerReport` to the server the
  message arrived on: pack id, index, `k_i`, `k_0`, a reason (rights,
  illegal content, abuse, other) and up to 1,000 characters; "Include the
  whole pack" adds the pack key.
- **Evidence:** the server recomputes `c_i` from `k_i` and finds `h_i` in
  its stored manifest. A match proves that this key opens that object,
  first uploaded there by account X at time T under licence hash H.
- **Review:** the server's moderators see, in the client's server
  administration, the reported stickers (decoded in their content worker),
  the manifest, the whole pack if included, the uploader's account, licence
  record and earlier reports. Other packs' contents and who used a sticker
  in E2EE conversations stay hidden.
- **Removal:** the server deletes the blobs and blocks the pack id and its
  hashes, so `StickerHave` reports them blocked and re-uploads fail. Its
  clients show "Sticker unavailable on this server"; other servers are
  unaffected. Anyone can also hide a pack in their own client.
- **Uploaders** get a statement of reasons and one appeal (DSA Art. 17, 20).
  Three upheld removals in twelve months suspend that account's uploads on
  that server.
- **nuschl.eu:** servers the project runs are operators like any other: they
  receive the grant, keep the records and handle reports, possibly in
  triage.nuschl.eu (#205) (D11).

## 8. Sending and showing stickers

### 8.1 The reference

```proto
message StickerRef {
  bytes pack_id = 1;   // 32, h_0
  bytes pack_key = 2;  // 32
  uint32 index = 3;    // 1..=200
  bytes object = 4;    // h_i, 32
  string emoji = 5;    // one emoji of the table
  string alt = 6;      // <= 120 bytes
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

On stock servers the visible message is an `<img>` of the first frame from
the content worker's existing `EncodeImage`, within the server's image
limit, then the emoji. The annotation links to it by `msg_id` and text
digest, as #216 §2.4 does, and Nuschl receivers show that still as the
sticker. "Save pack" asks the sender's client over message 26; while the
sender is online it answers for packs it sent there, through the existing
magic-wormhole transfer worker, and the receiver verifies by pack id (D9).

### 8.2 Rendering and accessibility

- The driver decrypts and the content worker decodes. Desktop rows show
  stickers at 160 logical px; the web client decodes in the content WASM
  worker and draws validated pixels.
- Animation plays while visible, holds the first frame under reduced
  motion, and plays on hover or focus.
- A refused or removed pack shows the emoji and "Sticker unavailable on
  this server".
- Accessible name: "Sticker: <alt text, or the emoji's name>", with the pack
  title as description. Copying copies the emoji; search finds stickers by
  emoji name and alt text.

## 9. The picker (#250)

The Stickers tab replaces "Stickers are coming soon." with the library:

- Pack tabs (cover images) like the emoji category row: Recent, the
  library's packs, then "+" (Add pack: link or files).
- Cell ids are grid positions, as #250 uses for emoji; cells carry the
  accessible names of §8.2 and arrow-key navigation.
- Search uses #250's names and keywords of each sticker's emoji, plus alt
  text.
- Click sends and closes; Shift+click sends and keeps the picker open;
  Escape closes. The first send on a server shows "Uploading pack" in the
  row until the sticker is up.
- Empty state: "No sticker packs yet" and "Add pack". A received sticker's
  menu has "Save pack" and "Report sticker".
- Recent stickers last until quit, as recent emoji do. The web picker
  matches; adding from links and files is desktop only (D12).

## 10. Slices

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
| S9 | Stock "Save pack" through the transfer worker | S7 | M |
| S10 | Web: library, picker, rendering, sending | S1, S7 | M |

## 11. Tests

- Vectors for keys, commitments, hashes and blobs; a blob in another slot, a
  manifest listing a wrong hash and a GCM ciphertext built to open under two
  keys are refused.
- A synthetic external pack built from the spec by a test encoder, served by
  a local CDN fixture in a VM; tests contact local fixtures only. Counted
  requests: a library hit makes 0, a server hit 0 or 3, a miss one per
  distinct file, a resume none; `Retry-After` is honoured.
- Two nuschld VMs: a sticker sent on A uploads to A only; after saving, a
  send on B uploads once and a second send uploads nothing; quota and hash
  failures are refused; removal on A leaves B serving; an E2EE server holds
  only ciphertext; expired references delete blobs.
- Content worker: a declared 65535 x 65535 PNG, a 10,000-frame APNG, long
  WebP, truncated files, MIME mismatch.
- Desktop smoke in headless GNOME Shell (as `tests/emoji_smoke.py`): add,
  pick, send, render, AT-SPI names.
- Stock Mumble in a VM shows the inline image; a Nuschl client shows one
  sticker, once, and "Save pack" verifies the transferred pack.

## 12. Open questions (the spec does not say)

1. Does the external CDN use a publicly trusted certificate? If it needs the
   private CA the spec mentions, it would be pinned for that host only.
2. Its rate limits, and whether it sends `Retry-After`.
3. Whether it expects particular request headers.
4. Whether its terms allow third-party clients to fetch packs.

## 13. Decisions for the owner

1. **Hosting:** per server, uploaded on first use (recommended). The
   alternative, one central host (`stickers.nuschl.eu` or `art.nuschl.eu`),
   gives one moderation point, one licensee, references valid on every
   server and CDN caching; its operator carries every pack's legal duties,
   sees every fetching IP and is a single point of failure.
2. **Certification:** once per pack at library entry (recommended; rights
   are a fact about the content), or once per server at first upload, which
   names each new operator and asks more often.
3. **First upload:** the whole pack in the background (recommended;
   recipients can save it) or only the stickers sent.
4. **AEAD:** ChaCha20-Poly1305 (recommended) or AES-256-GCM, both from
   libcrux with the §2.2 commitment.
5. **Dedupe hit:** verify with 3 external requests (recommended) or trust
   the first importer with none.
6. **Lookup scope:** every connected Nuschl server (recommended; each sees a
   keyed tag only) or one chosen home server.
7. **External hosts:** two constants in `external_pack` plus an off switch
   (recommended), or a configuration file.
8. **Pack key on stock and non-E2EE servers:** send it (recommended;
   receivers can save the pack) or the sticker key only.
9. **Stock "Save pack":** from the sender's client (recommended) or not
   offered on stock servers.
10. **Server defaults:** 1 GiB and 100 packs per user, 20 GiB per server,
    stickers on by default.
11. **nuschl.eu's servers:** whether they accept stickers, and whether their
    moderation uses triage.nuschl.eu.
12. **Scope:** adding from links and files on the desktop only in v1.
13. **Licence:** the binding language for translations, and three years of
    record retention after hosting ends.
14. **Shared blocklists** of pack ids between operators: design later, or
    keep each operator's list its own.
