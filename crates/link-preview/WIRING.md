# Link previews, slice A: wiring into shared files

Status, 2026-11-13: the offline pieces below live in `crates/link-preview` and are
unit-tested (no network, DNS, TLS or socket code). Nothing outside this crate uses
them yet. This file lists the shared-file hunks that connect them, for the
integrator. It refines `docs/link-previews-plan.md` (commit `5ca6e749`); where it
differs from the plan, the difference is listed under "Deviations".

## What exists in this crate

| Module | Public API | Plan item |
|---|---|---|
| `extract` | `extract_open_graph(&[u8], TransportCharset) -> Result<ExtractedMetadata, ExtractError>`, `TransportCharset::{Utf8, Unspecified, Other}` + `from_label`, `ExtractedMetadata { title, description, image, image_alt }`, `ExtractError::{TooLarge, Encoding, NoMetadata}`, limit constants | 4.1 extraction (content worker only) |
| `card` | `Card::new(&Target, &ExtractedMetadata, Option<EmbeddedImage>)`, `Card::from_preview(&Preview)`, `to_preview`, `is_for`, accessors, `without_image`, `weight`; `check_image`, `image_dimensions(ImageMime, &[u8])`; `CardError` | card model, `ImageMime::from_magic` equivalent |
| `projection` | `project(text_html, &Card, &ServerLimits) -> Projection::{Card { html, description, image }, TextOnly(TextOnly)}` | 6 legacy projection |
| `xml` | `well_formed(&str)`, `is_xml_char` | projection guard |
| `draft` | `DraftPreviews<K>`: `set_policy`, `update(Draft, now)`, `deliver(RequestId, Result<Card, Unavailable>, now)`, `for_send(Draft)`, `sent`, `forget`, `remove`, `restore`, `status`; `Draft`, `Ticket`, `Effect::{Fetch, Cancel}`, `Status`, `Ineligible`, `Unavailable`, `Delivery` | SDK `DraftPreviews` core |
| `settings_file` | `parse(&impl Codec, Source) -> Loaded`, `serialize`, `effective(user, managed)`, `Loaded { settings, present, problem }`, `Loaded::may_overwrite`, `Problem`, `Codec`, `FILE_NAME`, `MANAGED_FILE_NAME` | 5 settings sidecar |
| `text` | `normalize`, `is_normalized`, `truncate_utf16`, `utf16_len`, `refused` | shared normalisation |

## Shared-file hunks (none made here)

Order: manifests first (Root), then ipc/worker, then SDK. Line numbers are at main
`c3cae6f0`.

1. **Manifests and graph (Root).** Edges only, no new crates or features:
   - `apps/content-worker/Cargo.toml` `[dependencies]`: `nuschl-link-preview.workspace = true`
     (default features; the worker calls `extract::extract_open_graph`).
   - `crates/client/Cargo.toml` `[dependencies]`: `nuschl-link-preview.workspace = true`.
     Its dependencies (nuschl-core, ipnet, serde, thiserror, url) build for wasm32.
   - `fuzz/Cargo.toml`: `nuschl-link-preview = { path = "../crates/link-preview" }`, two
     `[[bin]]` entries (item 9), and the cut-down fuzz lock.
   - Then `just crate2nix`.
2. **`crates/ipc/src/preview.rs` (new) and `crates/ipc/src/lib.rs`.** In `lib.rs`:
   `pub mod preview;`, `ContentRequest::Preview(preview::PreviewContentRequest)` (enum at
   `:44`), `ContentResponse::Preview(preview::PreviewContentResponse)` (enum at `:82`).
   Sequence after the musicbot `ContentRequest::Thumbnail` variant. `preview.rs` holds
   wire types only (ipc must not link the scanner):
   - `PreviewContentRequest::Document { schema: u16, body: String /* base64 */,
     charset: WireCharset /* utf8 | unspecified | other */, truncated: bool }`;
   - `PreviewContentResponse::{Metadata(WireMetadata), Unavailable(WireUnavailable)}`,
     `WireMetadata { title, description, image: Option<String>, image_alt }` mirroring
     `extract::ExtractedMetadata`, `WireUnavailable::{NoMetadata, Encoding, TooLarge, Malformed}`;
   - validators: base64 length checked before decoding (decoded ≤
     `extract::MAX_DOCUMENT_BYTES`), reply strings ≤ the carried limits, the image
     reference ≤ 2048 bytes, a preview reply frame cap (plan: 96 KiB) re-checked after
     `host::exchange`. `deny_unknown_fields` everywhere; unknown `schema` fails.
   - `Image`/`Verify`/`DnsAnswer` variants follow with the image derivative policy
     (pending) and slice C; they are not part of this crate's work.
3. **`apps/content-worker/src/main.rs`.** One arm next to the existing ones (`:42-63`):
   decode the body, call
   `nuschl_link_preview::extract::extract_open_graph(&bytes, charset)` and map the
   result to `ContentResponse::Preview(..)`. The worker keeps its existing sandbox,
   one-shot lifetime and 5 s budget; no new argv or mode in slice A.
4. **`crates/legacy-html/src/lib.rs`.** `pub fn link_ranges(text: &str) -> Vec<(usize, usize)>`,
   a public wrapper of the private `links` (`:52`), so the SDK picks the same first web
   link stock Mumble shows as a link. The projection builder stays in this crate (it
   needs `Card` and `ServerLimits`; legacy-html stays dependency-free).
5. **`crates/client/src/lib.rs` and `crates/client/src/preview.rs` (new).** `pub mod preview;`.
   The new file owns a `DraftPreviews<ConversationKey>`, candidate detection (first
   `https://` range from `link_ranges`, terminated by whitespace or pasted, plus the
   750 ms debounce, with the draft capped at `draft::MAX_DRAFT_BYTES`), and the carry
   decision: `Ineligible::HtmlDisabled` (`ServerLimits::allow_html` false),
   `ExtensionConversation` (Nuschl history conversations before `preview/1`, D4),
   `EndToEnd` (`e2ee/1`), `Attachments` (attachments queued).
6. **`crates/client/src/platform.rs`** (`trait Platform` at `:128`): two methods with
   default bodies, so `client-wasm` and test platforms need no change:
   `fn link_preview(&self, ticket: nuschl_link_preview::draft::Ticket<ConversationKey>) -> Result<(), String> { Err("link previews are unavailable here".into()) }`
   and `fn cancel_link_preview(&self, id: nuschl_link_preview::draft::RequestId) {}`.
   `Effect::Cancel` must not release process credit before the worker has exited
   (native implementation, slice B).
7. **`crates/client/src/input.rs`** (`enum Input` at `:28`):
   `Input::LinkPreview { id: RequestId, result: Result<Card, Unavailable> }`; the
   dispatch calls `DraftPreviews::deliver` with a monotonic `now` (the SDK without
   `native` has no clock: the frontend passes it, or the native driver stamps it).
8. **`crates/client/src/client.rs`.** A `previews` field on `Client`; entry points
   `preview_draft(conversation, text, pasted)` → `update` + perform effects through the
   platform, `remove_preview(conversation)` → `remove`, `restore_preview`; on every
   connect/disconnect and policy change pass the new generation (`update` sees the
   connection generation in `Draft`; `set_policy` for settings). In `send_text`
   (`:1096`): `if let Some(card) = previews.for_send(draft)`, build
   `text_html = nuschl_legacy_html::legacy_body(&body, true)` and
   `projection::project(&text_html, card, &limits)`; `Projection::Card { html, .. }` goes
   out as one stock `TextMessage` (the `SendHtml` path), `TextOnly(_)` sends exactly as
   today. `PendingSend` (`:277`) gains `wire: String` so the session's echo matches the
   HTML while `Update::Sent` still carries the draft text; call `previews.sent(..)` after
   a successful send. Never reuse the attachment functions (no file-offer fallback).
9. **Fuzz targets (new files, Root manifest).** `fuzz/fuzz_targets/preview_html.rs`:
   `extract_open_graph` over arbitrary bytes and charsets; assert the field limits and
   `text::is_normalized`. `fuzz/fuzz_targets/preview_projection.rs`: arbitrary card
   strings and `ServerLimits` into `project`; assert `xml::well_formed`, the text prefix
   and the limits. Seeds under `fuzz/seeds/`.
10. **`flake.nix` checks (Root):**
    `link-preview-unit = crates.project.workspaceMembers."nuschl-link-preview".build.override { runTests = true; };`
11. **Settings files (desktop/TUI or `crates/session` config, owner to decide).** Read
    `link-previews.json` and `managed-link-previews.json` with a bounded read
    (`settings_file::MAX_FILE_BYTES` + 1, like `read_bounded`), pass `Source` and a
    serde_json `Codec` (five lines), apply `effective(&user, &managed)` and hand the
    settings to `DraftPreviews::set_policy`. Write atomically with 0600 like the
    voice-quality sidecar, never over a file whose `Loaded::may_overwrite()` is false
    unless the user explicitly saves new settings. Show `Problem` once.
12. **Documentation (Root):** `docs/link-previews.md` status, a SECURITY.md fuzzing row
    for `preview_html` and `preview_projection`, and the note that extraction runs only
    in the content worker.

`crates/core/src/preview.rs` needs no change: the plan's
`EmbeddedImage::legacy_img_element` and `ImageMime::from_magic` are covered by
`projection` (private `img_element`, the same format as nuschl-content's) and
`card::image_dimensions`.

## Dependency edges this crate could use (not added)

- `nuschl-legacy-html.workspace = true` in `crates/link-preview/Cargo.toml`: `project`
  could take the draft text and call `legacy_body(text, true)` itself, removing the
  contract that callers pass exactly that string.
- `serde_json` moved from `[dev-dependencies]` to `[dependencies]`: the `Codec` trait
  in `settings_file` would become unnecessary.
- `html5ever` (already locked for nuschl-content): the plan's tokenizer. The scanner
  here is linear, safe Rust and needs no edge; switching is optional.

## Deviations from the plan

- Extraction is a hand-written head-only scanner, not the html5ever tokenizer: this
  crate has no html5ever edge. It still runs only in the content worker.
- The projection has no `alt` attribute (the plan's template has none either); the
  alternative text travels only in the typed model.
- The SDK keeps a small result memory (16 entries, 1 MiB, 10 min / 2 min) with a
  caller-supplied clock, so edits that keep or return to a URL need no fetch. The
  supervisor cache of slice B is separate.
- A present managed sidecar decides alone (Off when unusable); the plan's "cap" form
  (D8) is not implemented.
- Non-UTF-8 pages give no preview (D11), including ASCII pages labelled windows-1252.

## Bounds

| Resource | Bound | Where |
|---|---|---|
| Document | ≤ 256 KiB, else `TooLarge` | `extract::MAX_DOCUMENT_BYTES` |
| Tags scanned | ≤ 4096, then stop | `extract::MAX_TAGS` |
| `<meta>` tags | ≤ 256, then stop | `extract::MAX_META_TAGS` |
| Attributes per tag | first 64 used, rest consumed | `extract::MAX_ATTRIBUTES` |
| Attribute value / title source | ≤ 4 KiB read | `extract::MAX_VALUE_BYTES` |
| Entity decoding output | ≤ 8 KiB per field | `extract` |
| Title / description / alt | ≤ 512 / 2048 / 512 B, char boundary | `nuschl_core::preview` limits |
| Image reference | ≤ 2048 B, dropped (never cut) beyond | `MAX_URL_BYTES` |
| Card host | ≤ 253 B, ASCII (punycode) | `card::MAX_HOST_BYTES` |
| Card image | ≤ 32 KiB, ≤ 640×360, header must match | `card::check_image` |
| Projection title / description | ≤ 200 / 300 UTF-16 units shown | `projection` |
| Projection size | Murmur text limit (+64 margin, sources excluded), image limit, 256 KiB client bytes | `projection::project` |
| XML nesting / attributes | ≤ 64 / ≤ 16, else text only | `xml` |
| Draft text | ≤ 256 KiB, else no preview | `draft::MAX_DRAFT_BYTES` |
| Drafts | ≤ 64 conversations (LRU) and ≤ 1 MiB kept text | `draft` |
| Requests in flight | 1; a newer one cancels | `draft::MAX_IN_FLIGHT` |
| Result memory | ≤ 16 entries, ≤ 1 MiB, cards 10 min, failures 2 min, cleared on policy change | `draft` |
| Settings file | ≤ 4 KiB read and written | `settings_file::MAX_FILE_BYTES` |
