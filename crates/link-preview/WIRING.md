# Link previews, slice A: wiring into shared files

Status, 2026-11-13 (correction lane C099, on main `733cc124`). The offline pieces
below are unit-tested; nothing else uses them yet. No network, DNS, TLS, socket,
image conversion, SDK, IPC, worker or UI code exists. This note lists the remaining
shared-file hunks for the integrator; it refines `docs/link-previews-plan.md`.

## What exists

| Where | Public API |
|---|---|
| `nuschl_content::preview` (content worker and content WASM only) | `extract_open_graph(&[u8], TransportCharset) -> Result<ExtractedMetadata, ExtractError>` on html5ever's tokenizer; `transport_charset(Option<&str>)`; re-exports `ExtractedMetadata`, `ExtractError`, `TransportCharset`, `MAX_DOCUMENT_BYTES`; limits `MAX_TAGS`, `MAX_META_TAGS`, `MAX_ATTRIBUTES`, `MAX_VALUE_BYTES` |
| `nuschl_link_preview::metadata` | the bounded value: `ExtractedMetadata { title, description, image, image_alt }` + `is_bounded()` (check worker replies with it), `TransportCharset`, `ExtractError` |
| `::card` | `Card::new(&Target, &ExtractedMetadata, Option<EmbeddedImage>)`, `Card::from_preview`, `to_preview`, `is_for`, accessors; `check_image`, `image_dimensions` |
| `::projection` | `project(draft_text, &Card, &ServerLimits) -> Projection::{Card { html, description, image }, TextOnly(TextOnly)}`; calls `legacy_body(text, true)` itself; `is_xml_char` |
| `::draft` | `DraftPreviews<K>`: `set_policy`, `update`, `deliver`, `for_send`, `sent`, `forget`, `remove`, `restore`, `status` |
| `::settings_file` | `parse(Source) -> Loaded`, `serialize(&Settings)`, `effective(&user, &managed)`, `cap(&user, &managed)` |

Manifest edges in place: nuschl-content → nuschl-link-preview,
nuschl-link-preview → nuschl-legacy-html, serde_json a normal dependency of
nuschl-link-preview; `fuzz/Cargo.lock` refreshed for them. The content worker and
the web WASM closures now include the third-party `ipnet` 2.12.2 (MIT OR
Apache-2.0); release notices pick it up from the graph.

## Remaining shared-file hunks (none made here)

Line numbers at `733cc124`; owners as in the plan.

1. **`crates/ipc/src/preview.rs` (new) + `crates/ipc/src/lib.rs`**: `pub mod preview;`,
   `ContentRequest::Preview(..)`, `ContentResponse::Preview(..)` (after musicbot's
   `Thumbnail`). Wire types only: `Document { schema, body /* base64, decoded ≤
   MAX_DOCUMENT_BYTES */, charset, truncated }` → `Metadata { title, description,
   image, image_alt }` | `Unavailable(NoMetadata | Encoding | TooLarge | Malformed)`.
   Check base64 length before decoding, a preview reply frame cap after
   `host::exchange`, and the reply with `ExtractedMetadata::is_bounded()` before use.
2. **`apps/content-worker/src/main.rs`**: one arm calling
   `nuschl_content::preview::extract_open_graph`. No manifest edge needed (the types
   are re-exported by `nuschl_content::preview`).
3. **`crates/legacy-html/src/lib.rs`**: `pub fn link_ranges(text) -> Vec<(usize, usize)>`
   wrapping the private `links`, for picking the candidate link.
4. **SDK (`crates/client`)**: edge client → nuschl-link-preview; a new `preview.rs`
   owning `DraftPreviews<ConversationKey>` (candidate: first `https://` range, terminated
   or pasted, 750 ms debounce; carry decision → `Ineligible`); `Platform::link_preview`
   and `cancel_link_preview` with default "unavailable" bodies (`trait Platform`);
   `Input::LinkPreview { id, result }`; in `Client::send_text`: `for_send`, then
   `project(&body, card, &limits)`, sending `Projection::Card { html }` as one stock
   `TextMessage` (`PendingSend` gains `wire`), `sent()` afterwards. Never the
   attachment path; no image ever becomes a file offer.
5. **Settings I/O** (session config or frontends): bounded read (`MAX_FILE_BYTES` + 1),
   `parse`, `effective`, `set_policy`; atomic 0600 writes, never over a file whose
   `Loaded::may_overwrite()` is false unless the user explicitly saves.
6. **`flake.nix` checks (Root)**: `link-preview-unit` and a content unit leaf, each
   `.build.override { runTests = true; testInputs = [ pkgs.python3 ]; }` for
   link-preview: its projection test parses the output with Python's expat.
7. **Fuzz (Root manifest)**: `preview_html` over `nuschl_content::preview` (no new
   edge), `preview_projection` over `projection::project` (fuzz → nuschl-link-preview
   edge).
8. **Docs (Root)**: `docs/link-previews.md` status; a SECURITY.md fuzzing row; the note
   that extraction runs only in the content worker (and the content WASM worker,
   which is not an OS sandbox).

## Decisions taken here, for codex

- Managed sidecar: `cap` makes the mode the narrower of user and managed
  (Off < ProxyRequired < Direct), so it never loosens Off; a managed proxy or resolver
  replaces the user's. An unreadable, malformed or newer managed file, or one naming an
  invalid proxy or resolver, means Off with a `Problem`; ProxyRequired without a
  complete configuration fetches nothing and says why. The mode rule is isolated in
  `cap`; the caller must pass only the managed file from administrator locations.
- Strict UTF-8: other declared encodings give no preview.
- The projection carries no `alt` attribute; alternative text stays in the typed model.

## Bounds

| Resource | Bound |
|---|---|
| Document | ≤ 256 KiB, else `TooLarge` |
| Tags + comments + doctypes / `<meta>` tags | ≤ 4096 / ≤ 256, then stop |
| Attributes per `<meta>` | first 64 |
| Attribute value / title text read | ≤ 4 KiB |
| Title / description / alt | ≤ 512 / 2048 / 512 B, char boundary |
| Image reference | ≤ 2048 B, dropped beyond |
| Card host / image | ≤ 253 B ASCII / ≤ 32 KiB, ≤ 640×360, header must match |
| Projection title / description shown | ≤ 200 / 300 UTF-16 units |
| Projection size | Murmur text limit (+64, image sources excluded), image limit, 256 KiB |
| Draft text / drafts / kept text | ≤ 256 KiB / ≤ 64 / ≤ 1 MiB |
| Requests in flight | 1 |
| Result memory | ≤ 16 entries, ≤ 1 MiB, 10 min / 2 min, cleared on policy change |
| Settings file | ≤ 4 KiB |
