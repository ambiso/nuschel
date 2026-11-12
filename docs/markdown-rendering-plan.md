# Markdown rendering in messages: plan

Status: plan only, 2026-11-12. Nothing here is implemented. It revises the
narrow "inline subset" sketch from the Markdown diagnosis: messages get
CommonMark through the parser the content worker already runs, with the
syntax hidden, newlines kept as typed, and code, math, links and resources
under the rules they follow today. The one prerequisite has landed in a
separate lane: the native desktop now draws bold, italic and monospace at
default fonts (the GPUI font-fallback fix); without it, bold spans are
invisible on the desktop whatever this plan does.

## Today

**Parser.** `pulldown-cmark` 0.13.6 (`default-features = false`) is a
dependency of `crates/content` and of `fuzz` only. It is used in two places:

- `ContentRenderer::render` (`crates/content/src/lib.rs:370`) walks its events
  and acts on fenced code blocks alone (`Tag::CodeBlock(CodeBlockKind::Fenced)`),
  plus fences with the language `math`. Everything else is ignored, so message
  text is literal: the doc comment, `docs/desktop.md` ("Ordinary text is
  literal"), `docs/math.md` ("Nuschl does not render the rest of Markdown") and
  the `Format::TEXT` comment in `crates/protocol/proto/nuschl.proto:121`
  ("fenced code blocks are the only markup") all say so.
- `math::tokens` (`crates/content/src/math.rs:73`) parses twice: once without
  math to mask code spans, code blocks, autolinks, inline/block HTML and the
  linkifier's bare URLs byte for byte; then the masked text with
  `Options::ENABLE_MATH`. Formula tokens are accepted only if their content is
  exactly the bytes between the delimiters.

Legacy HTML does not go through pulldown-cmark: `markup::legacy_text`
(`markup.rs:513`, html5ever) turns stock Mumble HTML into "marked text":
visible text with `EMBED` (U+FFFC) directives for styles (`b i u s code
color: link:`), `h1`–`h3`, `item:DEPTH:MARKER`, `quote`/`/quote`, `hr` and
`data:` images. `markup::expand` (`markup.rs:1026`) turns marked text into
blocks and spans, applies the limits (`MAX_SPANS` 2048, `MAX_BLOCKS` 512,
`MAX_OPEN` 16, `MAX_LIST_DEPTH` 4, `MAX_LINKS` 128, `MAX_URL_BYTES` 2048),
runs `link_target` on every link and adds bare-URL links (`bare_urls`,
`markup.rs:588`). Plain message text goes through `expand` too, so typed text
is interpreted by the same machinery (markers included: see *Storage markers*).

The outbound copy for stock clients (`crates/legacy-html`: `legacy_body`,
`text_html`, its own linkifier) escapes text, keeps line breaks as `<br/>` and
links URLs. It does not involve pulldown-cmark and stays unchanged.

**Document model** (`crates/core/src/lib.rs`), already serving legacy HTML:

| Model | Has | Missing for CommonMark |
| --- | --- | --- |
| `Style` | `bold`, `italic`, `underline`, `strike`, `code`, `color` | nothing for inline emphasis or code |
| `Span` | byte range, `style`, `link` (allowed schemes only), `math` | link titles (dropped by design) |
| `TextRole` | `Paragraph`, `Heading(1..=3)`, `Item { depth, marker }`, `Quote`, `Rule` | heading levels 4–6; quote depth; a role inside a quote (an item or heading in a quote has one role, not both); task-list state |
| `ContentBlock` | `Text`, `Code { language, runs }`, `Image` (decoded pixels), `Math` | nothing needed; tables have no block (legacy draws rows as `a │ b`) |

Every frontend draws all of it: desktop `apps/desktop/src/document.rs`
(`highlight` for span styles, `text_block` for roles), TUI
`apps/tui/src/document.rs:52` (heading `#` prefix and bold, item marker and
indent, `> ` quote, rule line, bold/italic/strike/underline; inline `code` has
no distinct look) and web `web/app/src/TypedDocument.svelte` (all styles and
roles; validated in `web/app/src/content-protocol.js`). So styles and roles
need no model or frontend change. Keeping lines exactly does: the model has
no way to say how many line breaks separate two blocks (*Lines*).

**Which text is rendered where.** `render` is the single place for every
path, and stored history keeps the source, not the document:

| Path | Input to the worker | Classification |
| --- | --- | --- |
| Typed (local echo) | the typed source, `IncomingText { html: false }` (`crates/session/src/connection.rs:3215`, stock path `3571`) | `ContentRequest::Render` (`crates/ipc/src/host.rs:131`) |
| Received on nuschld | `Message.content`, `html = format == LegacyHtml` (`connection.rs:3393`) | `Render` for `TEXT`, `LegacyHtml` otherwise |
| Received on stock Murmur | the HTML, `html: self.allow_html` (`connection.rs:3977`), including other Nuschl clients' escaped copies | `LegacyHtml` → `legacy_text` → `render` |
| Restored, local | stored body: the source for `Render`, the marked text for `LegacyHtml` | `render_many` (`crates/client/src/native.rs:2309`) |
| Restored, server pages | `prepare(content, format == LegacyHtml)` (`native.rs:115`) | as received |
| Welcome, comments, channel descriptions | stock HTML (`prepare(body, true)`) | `LegacyHtml` |
| Web | `ContentProcessor::render_message(source, mode)` (`crates/content-wasm/src/lib.rs:52`) | the same `render` |

Local storage has no format column, so a restored message is re-rendered from
its stored text alone. A Markdown pass that runs only for some formats would
need the format carried into storage and into `ContentRequest`, an IPC and
storage shape change. This plan therefore puts Markdown inside `render` and
applies it to every text that reaches it (decision D1).

## Intended behaviour

A message is CommonMark (pulldown-cmark, plus the math grammar of
`docs/math.md` and GFM strikethrough) with these rules:

1. **Syntax is hidden.** The delimiters of every construct that is rendered
   are removed from the visible text: emphasis runs, code-span backticks,
   link brackets with destination and title, autolink angle brackets, ATX
   `#` runs, setext underlines, list markers, `>` markers, thematic-break
   lines, fence lines (already), indented-code indentation, backslash escapes
   and hard-break backslashes. A construct that is not rendered (see the list
   below) keeps all of its bytes.
2. **Lines are kept exactly (a user requirement).** Every line break typed
   is a line break and every blank line is a blank line, inside paragraphs and
   next to headings, list items, quotes, rules, code and math alike, in the
   desktop, the TUI and the web: no frontend adds or drops a line. A soft
   break is drawn as a hard break. Only syntax-only lines (fence lines, setext
   underlines) are drawn by their block instead. See *Lines*.
3. **Code and math bind tighter, as today.** Code spans and blocks, then
   formulas, then autolinks, HTML and bare URLs are found first and masked;
   emphasis and links are found in what is left (*Interactions*).
4. **Links only to allowed targets.** A link is rendered only if
   `markup::link_target` accepts the destination (http, https, mumble; no
   control or invisible characters; at most 2048 bytes) and fewer than
   `MAX_LINKS` links exist; otherwise the whole construct stays literal, so
   nothing that was refused is hidden. Titles are dropped.
5. **Resources stay inert.** Image syntax `![alt](src)` is shown literally and
   never fetched or decoded (the linkifier may link a bare URL inside it, as
   today). Raw HTML, inline or block, is literal text. Images still come only
   from stock `<img src="data:…">` through the existing legacy path.
6. **Outbound is unchanged.** `legacy_body`/`text_html` still send the source
   escaped; stock Mumble users see the Markdown as typed. Stock Mumble 1.4+
   senders convert Markdown typed in their chat bar to HTML before sending
   (Mumble's own feature; not re-verified for this plan), which reaches Nuschl
   as legacy HTML and renders as today.

**Rendered** (CommonMark core unless noted): emphasis and strong emphasis
(`*`, `_`, CommonMark's flanking rules, so `snake_case` stays literal and
`__init__` becomes strong "init", as everywhere else); code spans; inline
links with allowed destinations; URI autolinks; ATX headings (levels 4–6 are
drawn as level 3); setext headings (D5); thematic breaks; block quotes; bullet
and ordered lists (CommonMark numbering from the start number, nesting up to
depth 4); fenced code (highlighted as today); indented code (D4); backslash
escapes; hard breaks; strikethrough `~~text~~` (GFM extension, D6); inline and
display math as in `docs/math.md`.

**Not rendered, shown as typed:** raw HTML, images, reference links and their
definitions (D7), email autolinks (`mailto` is not an allowed scheme), entity
and numeric character references (D8), and the extensions that are not
enabled: tables, task lists, footnotes, definition lists, super/subscript,
wikilinks, smart punctuation, heading attributes, metadata blocks.

**Where this departs from CommonMark**, stated so nobody calls it "full
CommonMark" by accident:

| CommonMark | Nuschl |
| --- | --- |
| A soft line break is rendered as a space or newline at the renderer's choice | always a line break |
| Any number of blank lines separates two paragraphs once | every blank line is kept |
| Blank lines between blocks (around headings, lists, quotes, code) carry no meaning; a loose list is just spaced differently | every blank line is kept as a blank line, between any two blocks, and before the first and after the last |
| Leading whitespace of a paragraph's continuation lines is stripped | kept (it is not syntax there) |
| Entity references are decoded | kept literally (D8) |
| Raw HTML is passed through | literal text |
| Images are embedded | literal text |
| Any URI scheme in links | http, https, mumble; others literal |
| Reference links resolve against definitions, which produce no output | both literal (D7) |
| Headings 1–6 | 1–3 (4–6 drawn as 3) |
| Nested quotes, lists in quotes | one `Quote` role (nesting flattened); an item inside a quote is an item, its quote styling is lost (model gap, D10) |
| No math | `$…$`, `$$…$$` and `math` fences (`docs/math.md`) |

## Lines

### The rule

A message has as many lines as its source (`source.split('\n')`), each
drawn where it was typed:

- inside a block, its own line breaks (paragraph text, a multi-line list
  item, code, a display formula written over several lines);
- between two blocks, `k` line breaks in the source (from the end of one
  block's last line to the start of the next block; pulldown-cmark's block
  ranges include the line break that ends a block, which is counted here and
  not in the block) are drawn as the next block starting on the next line
  with `k - 1` blank lines in between; `k = 0` (a display formula written
  within a line) keeps both on one line;
- before the first block and after the last, `k` line breaks are `k` blank
  lines;
- a syntax-only line belongs to its block and is drawn by it, not as a line
  of text: the opening and closing fence lines (the code box and its
  toolbar), a setext underline (the heading), a thematic break (the rule,
  drawn in a line of its own height).

One exception, kept from today: the stock copy of a message that opens with
a fence carries one extra leading line break (`legacy_body` adds it so stock
Mumble starts the fence on its own line). It is removed where it is known to
be added, in `legacy_text` (a leading `<br/>` directly before a fence), not by
trimming every leading break in `render` as today. Stored legacy history from
before the change still contains it and shows one blank first line there.

### Today

Nothing carries line breaks between blocks except the text at a text block's
edges, and the frontends disagree about it:

- the worker leaves boundary breaks in text (`markup::Expand` keeps them;
  text after a fence is `"\nlast line"`) but drops whitespace-only paragraphs
  (`Expand::end_block`), so blank lines between two non-text blocks are lost;
- the desktop uses edge breaks only to decide whether a display formula
  joins its line (`apps/desktop/src/document.rs:173`), then trims all of them
  (`text_block`, `document.rs:256`), and stacks blocks with an 8 px gap
  (`apps/desktop/src/view.rs:2914`, `.gap_2()`), list items 4 px closer
  (`view.rs` `blocks`, `mt(px(-4.))`), rules with a 4 px margin;
- the TUI (`apps/tui/src/document.rs:52`) and the web
  (`web/app/src/TypedDocument.svelte`, `.text { white-space: pre-wrap }`) draw
  the edge breaks, so they show a blank line after every fence that the
  desktop does not, and the web adds margins around code (8 px) and formulas;
- `render` trims all leading line breaks; trailing ones stay in the last text.

### Model change (`crates/core`)

A new block kind carries the line breaks of a boundary:

```rust
pub enum ContentBlock {
    // … Text, Code, Image, Math as today …
    /// The line breaks typed between the neighbouring blocks (or before the
    /// first, or after the last): `count - 1` blank lines between two blocks,
    /// `count` at the start or end, 0 keeps a display formula on its line.
    Break { count: u32 },
}
```

- **Semantics.** Without a `Break`, two neighbouring blocks are one line
  break apart and nothing precedes the first or follows the last block, which
  is exactly today's layout, so every existing producer (images, avatars,
  icons, the viewer, welcome and comment documents, documents built by the
  clients) stays valid unchanged. The worker emits a `Break` wherever the
  count differs from that default: blank lines between blocks, `0` for a
  formula within a line, any count at the start or end.
- **Text edges.** A text block no longer begins or ends with a line break:
  the worker moves edge breaks into `Break` blocks, and a whitespace-only
  paragraph becomes a `Break`. Frontends then draw text verbatim.
- **Why a block kind, not a field.** A field on `MessageDocument` or on every
  block would change all 39 struct literals that build documents, in nine
  crates (`grep "MessageDocument {"`), several of them held by other owners;
  a variant changes only the code that matches on block kinds, which is the
  rendering this plan changes anyway (`_ =>` arms elsewhere already ignore
  it).
- **Limits.** `Break` blocks count toward a separate budget of
  `MAX_BLOCKS + 1` (at most one between two content blocks), so the content
  block limit is unchanged; `count` is at most the source's line breaks
  (bounded by `MAX_CONTENT_BYTES`). Frontends clamp what they draw for a huge
  count the same way they clamp a paragraph of newlines today (Show more).
- **Images.** Images are never inline: a `Break { count: 0 }` next to an
  image is drawn as one line break (today's behaviour, unchanged for the
  image owner).

### Frontends

All three draw: a block per line run, `count - 1` empty lines of the
message's line height for a `Break` between blocks (`count` at the edges),
no other vertical space between blocks.

- **Desktop.**
  - `apps/desktop/src/document.rs`: `Document::from` maps `Break` to a
    `Block::Break(count)` and derives "formula joins its line" from
    `Break { count: 0 }` instead of from text edges (`document.rs:173`);
    `text_block` stops trimming edge breaks (`document.rs:256`); `join` and
    `plain_text` (`document.rs:430`, Copy message, accessible names) join
    blocks with exactly their line breaks, so copying keeps the lines too.
  - `apps/desktop/src/view.rs` `blocks` (`view.rs:3519`): a `Block::Break`
    becomes an empty spacer of `(count - 1) ×` the message line height (or
    keeps the next formula on the line); the rule is drawn centred in a line
    of its own height; the list-item `mt(px(-4.))` goes. The message body
    column's `.gap_2()` (`view.rs:2914`) becomes no gap. Framed blocks (code,
    formula, image) keep their padding inside their own box.
  - `apps/desktop/src/selection.rs`: carets skip `Block::Break` (no text).
- **TUI.** `apps/tui/src/document.rs`: `Break` pushes `count - 1` empty lines
  (`count` at the edges); text no longer starts with a break, so the extra
  line after a fence disappears; a display formula with `Break { count: 0 }`
  on either side is drawn within the line as its source (today it gets a
  `[formula]` line of its own, which adds lines).
- **Web.**
  - `web/app/src/content-protocol.js` `validateDocument`: accept
    `{kind: "break", count}` (integer, at most the source size) with its own
    budget; today any unknown kind fails, so this ships together with the
    core change.
  - `web/app/src/TypedDocument.svelte`: a `break` is an empty element of
    `count - 1` line heights; blocks joined by `count: 0` share one line
    (a formula within a line); `web/app/src/app.css`: no margins between
    blocks (`.code-block`, `.formula` margins move inside their boxes).
  - `web/app/src/presentation.js` `visibleBlocks`: the collapsed preview
    counts a `break`'s lines toward its eight lines.

### Worker (when content is allocated)

`markup::Expand` (`markup.rs`): `end_block` stops dropping whitespace-only
paragraphs and stops leaving edge breaks in text; each boundary's breaks
become a `Break` when they differ from one (zero for a formula within a
line). The same applies to legacy marked text, whose `<p>`, `<br>` and block
breaks are lines too. `render` stops trimming leading breaks; `legacy_text`
removes the stock copy's added break before a leading fence.

## Implementation shape

One grammar, one pass, in the worker:

1. `render(source)` splits out fenced code (and indented code, D4) into
   `Piece::Block`s with the existing highlighter, recording each block's
   source range for the line counts (it no longer trims leading breaks; see
   *Lines*).
2. **Visible text.** For text that contains `EMBED` directives (legacy marked
   text, or typed text containing U+FFFC), Markdown runs on the visible parts
   only, with an offset map back; directives are left exactly where they
   are. A Markdown construct may span a directive (`**a<i>b</i>c**` makes
   `abc` bold and `b` also italic); a directive never becomes Markdown.
3. **Scan.** Generalise `math::tokens` into a shared scan (new
   `crates/content/src/markdown.rs`, `math.rs` calling it): stage one parses
   without math and records code spans/blocks, autolinks, HTML and bare URLs
   (masked as today, except that autolinks are recorded before masking so
   they can still be rendered); stage two parses the masked text with
   `ENABLE_MATH | ENABLE_STRIKETHROUGH` and yields formulas (validated by
   `Token::new` as today) and the Markdown structure. A link destination is
   read from the source at the masked destination's offsets, then
   `link_target`.
4. **Translate.** Build an in-memory marked text: visible bytes without the
   hidden ranges, with the existing directives inserted (`b`, `i`, `s`,
   `code`, `link:URL`, `h1`–`h3`, `item:DEPTH:MARKER`, `quote`/`/quote`,
   `hr`), formula ranges mapped through the same offset map. Two additions to
   `markup::Expand`: an explicit item end (`/item`), so a Markdown list item
   may contain line breaks (legacy items stay one line), and `Break`
   blocks at boundaries (*Lines*). This marked text is never stored: storage
   keeps the source.
5. **Expand** as today, which keeps one place for limits, link checks,
   bare-URL links, formula spans and blocks.
6. A text that is a file offer (`nuschl_core::transfer::parse_offer` on the
   visible text) is not translated: offers are detected from the document
   when the source is HTML (`crates/client/src/timeline.rs:135`), and a file
   name with `*` or `_` must not change.

## Interactions

- **Math.** Unchanged grammar and limits (`MAX_FORMULAS` 32, 4 KiB each).
  Formulas are atomic for Markdown: no delimiter inside a formula is
  emphasis, emphasis around a formula keeps it whole (`*$x$*` is an italic
  span containing a math span), a formula in a link's text still counts.
  `docs/math.md` "Links" row changes from "Nuschl shows Markdown links as
  typed" to rendering them.
- **Fenced code.** Unchanged: Syntect highlighting in the worker, `math`
  fences as display formulas, fence contents never interpreted. Code-span
  contents are kept byte for byte apart from CommonMark's one stripped space
  on each side.
- **Linkifier.** `bare_urls` still binds tighter than emphasis and math (a
  `_` or `*` inside a bare URL is not emphasis), and still adds links outside
  existing link spans; Markdown links count toward the same `MAX_LINKS`.
- **Storage markers.** Directives in stored legacy text are never parsed as
  Markdown (step 2). Typed text containing U+FFFC is interpreted by `expand`
  today (an existing behaviour, outside this plan); Markdown treats such
  segments as opaque too.
- **Legacy HTML input.** `legacy_plain_text` is unchanged; its visible text
  gets the Markdown pass (D1). That makes a Nuschl sender's message look the
  same to Nuschl users on stock Murmur (who receive it as escaped HTML) as on
  nuschld and in the sender's own echo. Literal `*` from old stock clients
  can become emphasis; Mumble 1.4+ clients convert their Markdown before
  sending (see rule 6).
- **Restored history.** Source and marked text are re-rendered by the same
  `render`, so live and restored documents are identical (as verified for
  today's behaviour). Existing `TEXT` history renders as Markdown after the
  change (D2).
- **TUI and web.** No change for styles and roles. Inline code in the TUI has
  no look of its own (add DIM or reverse video in `apps/tui/src/document.rs`).
  Lines: *Lines*, *Frontends*.
- **Copy and selection.** "Copy" and "Copy message" copy the rendered text
  (`apps/desktop/src/document.rs:430` `plain_text`,
  `selection.rs:194` `selected_text`, `menus.rs:840`), as they do for legacy
  HTML formatting today, so copying a bold word gives `word`; copied text
  keeps the message's lines, blank lines between blocks included. The source
  stays available where it matters: editing loads `message.source`
  (`apps/desktop/src/editing.rs:54`), search indexes the source without
  markers (`crates/storage/src/lib.rs:281`), notifications already use
  document text (`crates/client/src/client.rs:1974`). Search snippets show the
  source (`crates/client/src/search.rs:133`), so they would show `**word**`
  (D9). An explicit "Copy Markdown" entry in the message menu is an optional
  addition.
- **Accessibility.** Accessible names are built from document text, so
  delimiters disappear there as well (as for legacy formatting).
- **Fuzz invariant.** `fuzz/fuzz_targets/math_tokens.rs` ends with "the
  document's text, formulas included, is the message's"; hidden delimiters
  break it. New invariant (with a fuzz-only `markdown::layout(source)` that
  reports the hidden ranges and constructs):
  1. The document's visible text (text blocks, display formula sources, code
     contents) equals the source with exactly the hidden ranges removed,
     compared without line breaks as today.
  2. Every hidden range lies inside a construct that an independent parse of
     the same source reports, and contains only that construct's syntax
     (delimiter characters, marker digits, spaces, a destination and title,
     fence or setext lines).
  3. No hidden range intersects a formula token, code content, a bare URL,
     raw HTML or an `EMBED` segment.
  4. The existing formula invariants (shown as written, in order) hold.
  5. Every link target passes `allowed_link`, at most `MAX_URL_BYTES`, at most
     `MAX_LINKS`; no `Image` block comes from Markdown syntax.
  6. Text without Markdown syntax characters renders to itself (the old
     invariant, now a special case).
  7. **Lines.** For every two neighbouring content blocks, the line breaks
     between them in the document (a `Break`'s count, else 1) equal the
     line breaks in the source between their source ranges
     (`layout` reports each block's range); at the start and end, the
     document's breaks (a `Break`'s count, else 0) equal the source's
     leading and trailing line breaks; no text block begins or ends with a
     line break. Summed: the document's line breaks (text blocks, code
     contents, formula sources, `Break` counts and defaults) plus those on
     syntax-only lines that `layout` reports equal the source's line breaks.
     The legacy fuzz target checks the same for `legacy_plain_text` output,
     apart from the stock copy's leading break before a fence.

## Ownership and sequence

Held by other owners now, not touched by this plan:

- **Image owner** (content/IPC image encode and preview, desktop
  attachments/transfer/viewer): `crates/content/src/lib.rs` image functions
  (`decode_image`, `decode_icon`, `encode_image_message`, `decode_view`),
  `markup.rs` `Expand::image` and data-URI handling, `crates/ipc` content
  request shapes, `apps/desktop/src/{attachments,transfer,viewer,images}.rs`,
  offer detection in `crates/client/src/timeline.rs`.
- **Hash owner** (limits, the legacy formatter, notice setters):
  `crates/legacy-html`, content and message limits.
- **Web owner** (voice/status, then the native chat scrollbar): the
  conversation wrapper in `apps/desktop/src/view.rs`. The line layout changes
  `view.rs` `blocks` and the message body column next to it, so that edit
  waits for the scrollbar work or is made by the same owner.

Shared model and protocol, allocated together because each rejects the
other's output otherwise:

- **`crates/core`** (`ContentBlock::Break`): the document model every
  frontend, the IPC replies (`crates/ipc` `ContentResponse` carries
  `MessageDocument`; the image owner holds the IPC shapes) and the WASM
  JSON share. Struct literals stay valid; `match` arms on block kinds need
  the new variant (desktop `document.rs`, TUI `document.rs`, content-wasm
  `constrain_document`, the fuzz targets' matches).
- **Web protocol**: `web/app/src/content-protocol.js` `validateDocument`
  fails on any unknown kind, so `break` is accepted in the same change as the
  worker starts emitting it; `TypedDocument.svelte`, `presentation.js` and
  `app.css` draw it.

Content code (`crates/content`) stays frozen and read-only until codex
allocates it; the font fix stays as merged.

Steps (each its own commit and review):

1. **Decisions** D1, D2 and D4–D12 below (D3 is decided), recorded in
   `docs/math.md` and a new section of `docs/desktop.md`.
2. **Model and frontends for lines** (can precede the Markdown pass, since
   the line rule also fixes today's fence line): `crates/core`
   `ContentBlock::Break`; desktop `document.rs` (`From`, `text_block`,
   `join`), `view.rs` `blocks` and body column (coordinated with the web
   owner), `selection.rs`; TUI `document.rs`; web `content-protocol.js`,
   `TypedDocument.svelte`, `presentation.js`, `app.css`. Frontends must
   accept `Break` before the worker emits it.
3. **Content worker** (one owner, after the image owner has merged, since
   `lib.rs` and `markup.rs` are shared files, and only once codex allocates
   content): `markup.rs` `Expand` emits `Break` blocks and keeps
   whitespace-only paragraphs as breaks, `render` stops trimming leading
   breaks, `legacy_text` removes the stock copy's break before a leading
   fence; then new `crates/content/src/markdown.rs` (scan, translate, offset
   maps), `math.rs` `tokens` on the shared scan, `lib.rs` `render` calling it,
   `Expand` `/item`. No change to image functions, limits or IPC shapes.
4. **Spec table** in `crates/content` (input → blocks with role, visible text
   and spans), as `math.rs` `spec` does, with at least: `**b**`, `*i*`,
   `_i_`, `snake_case`, `__init__`, `2 * 3 * 4`, `**unclosed`, `` `**x**` ``,
   `$*x*$`, `*$x$*`, `[t](https://a.example)`, `[t](javascript:x)`,
   `[t](https://a.example "title")`, `<https://a.example>`, `<a@b.example>`,
   `![i](https://a.example/p.png)`, `<b>x</b>`, `# h`, `#tag`, `###### h6`,
   `- a\n- b`, `1. a\n1. b`, `3) a`, nested lists to depth 5, `> q\n> r`,
   `> - item`, `---`, `foo\n---`, `a\nb`, `a\n\n\nb`, `\*x\*`, `a\\\nb`,
   `&amp;`, `~~s~~`, `~s~`, four-space indented code, fences, `$$x$$`,
   legacy marked text with `*` inside a `link:` directive and around
   directives, typed U+FFFC, a file offer whose name contains `_x_`.

   Line rows (blocks, then the `Break` counts that must appear; none means
   the default):

   | Source | Blocks | Breaks |
   | --- | --- | --- |
   | `# T\ntext` | heading `T`, paragraph `text` | none |
   | `# T\n\ntext` | heading, paragraph | 2 between |
   | `# T\n\n\n\ntext` | heading, paragraph | 4 between |
   | `text\n\n# T` | paragraph, heading | 2 between |
   | `- a\n- b` | item `a`, item `b` | none |
   | `- a\n\n- b` | item `a`, item `b` (loose list) | 2 between |
   | `- a\n\n\n- b` | item, item | 3 between |
   | `- a\nb` | one item `a\nb` | none |
   | `> q\n\n> r` | quote `q`, quote `r` | 2 between |
   | `` ```\nx\n```\nafter `` | code `x`, paragraph `after` | none (no blank line in any frontend) |
   | `` ```\nx\n```\n\nafter `` | code, paragraph | 2 between |
   | `` a\n```\nx\n``` `` | paragraph `a`, code | none |
   | `` ```\nx\n```\n\n```\ny\n``` `` | code, code | 2 between |
   | `so $$x$$ and` | paragraph `so `, formula, paragraph ` and` | 0, 0 |
   | `$$x$$\n\ntext` | formula, paragraph | 2 between |
   | `---\n\ntext` | rule, paragraph | 2 between |
   | `Title\n===\n\ntext` | heading, paragraph | 2 between (the underline is the heading's) |
   | `\n\nhello` | paragraph `hello` | 2 at the start |
   | `hello\n\n` | paragraph `hello` | 2 at the end |
   | `a\n\n\nb` | one paragraph `a\n\n\nb` | none |
   | legacy `<p>a</p><h1>T</h1><p>b</p>` | paragraph, heading, paragraph | none |
   | legacy `<pre>x</pre><br/><br/>b` | code, paragraph | 2 between |
   | legacy stock copy `` <br/>```rust<br/>x<br/>``` `` | code | none (the added break is removed) |
5. **Fuzz**: new `fuzz/fuzz_targets/markdown.rs` with the invariants above,
   the line invariant (7) included; `math_tokens.rs` final assertion replaced
   by invariant 1; `html.rs` gains the line invariant for legacy text;
   `legacy_html.rs` unchanged and still passing.
6. **Small gaps**: TUI inline code look. Optional: "Copy Markdown" in
   `apps/desktop/src/menus.rs`; rendered search snippets in
   `crates/client/src/search.rs`.
7. **Docs**: `docs/math.md` (Syntax intro, Links row), `docs/desktop.md`
   (Messages), `docs/architecture.md:19,91`, `docs/browser-content.md`, the
   `Format::TEXT` comment in `nuschl.proto` (comment only), `docs/extension.md`
   §4 ("Nuschl Markdown").
8. **Rendered proof** (packaged desktop, private loopback servers, private
   GNOME Shell, null audio):
   - `tests/workers.py`: a `render` request with Markdown returns bold,
     italic, code and link spans, heading/item/quote/rule roles, and no
     delimiters in the text.
   - `tests/markdown_repro.py`: typed, received (text and stock HTML) and
     restored messages contain no `**`, `` ` ``, `[`/`](` in their accessible
     names; screenshots reviewed.
   - `tests/font_rendering.py`: a typed `**Wordmark Wordmark**` row has a
     bold/regular ink ratio of at least 1.3, typed `` `iiiiii` `MMMMMM` `` have
     equal code-background widths, live equals restored.
   - Lines, measured from pixels the same way (text rows found by ink): in a
     typed message `# T\n\n\ntext`, the paragraph row starts two message
     line heights below the heading row's line (±1 px); in
     `` ```\nx\n```\nafter ``, the paragraph row follows the code box with no
     blank line; in `- a\n\n- b`, one line height between the item rows;
     live equals restored.
   - `tests/tui_loopback.py`: the screen text shows no delimiters and the
     list markers and heading prefixes the TUI draws, and the screen has the
     source's lines for the table rows above (no line after a fence).
   - `tests/web_content.unit.mjs` / `web_content.browser.mjs`: the actual WASM
     renders Markdown source to the same document, `content-protocol.js`
     accepts `break`, and `TypedDocument` draws the bold, code and heading
     elements and `count - 1` blank lines for each `break`.
   - Unit tests: desktop `document.rs` (`From` with `Break`, `plain_text`
     joining with the source's lines), TUI `tests.rs` (lines per row of the
     table), web `presentation.test.js` (collapsed preview line counting).
   - Stock interoperability: `legacy_html` fuzz and the stock-client suites
     unchanged (the outbound copy does not change).

## Decisions for codex

- **D1 Where Markdown applies.** Recommended: everything `render` sees
  (typed, nuschld `TEXT`, stock HTML's visible text, restored history,
  welcome, comments, descriptions), because storage has no format and the
  alternative needs a format flag in `ContentRequest` and storage.
- **D2 Existing history.** Recommended: render existing `TEXT` messages as
  Markdown too (`docs/extension.md` already calls the format "Nuschl
  Markdown"); the alternative is a new `Format` value for new messages, a
  protocol change.
- **D3 Lines** (decided by codex, C019: a requirement, not an option). Single
  and blank lines are kept exactly everywhere, in all three frontends, through
  `ContentBlock::Break` (*Lines*). Left to confirm: the block-kind design over
  a field (it avoids 39 struct literals in nine crates), the stock copy's
  leading fence break as the one exception, and stored stock history from
  before the change showing that break as a blank first line.
- **D4 Indented code.** Recommended: render as code (CommonMark, and
  `math::tokens` already treats it as code); otherwise four-space indented
  lines stay literal.
- **D5 Setext headings** (`Title` over `===` or `---`). CommonMark renders
  them; in chat `text\n---` is often meant as a rule. Recommended: render
  (CommonMark), listed in the departures if not.
- **D6 Strikethrough.** Recommended: `~~text~~` only (GFM); single `~` stays
  literal; tables and task lists off.
- **D7 Reference links.** Recommended: literal (definitions would otherwise
  vanish from the message).
- **D8 Entity references.** Recommended: literal (no double decoding of
  stock HTML text).
- **D9 Copy and search snippets.** Recommended: copy the rendered text (as
  for legacy formatting today) and optionally add "Copy Markdown"; decide
  whether search snippets stay source or use the document.
- **D10 Model gaps.** Recommended: accept flattened quote nesting, items
  inside quotes without quote styling, and h4–h6 as h3 for now; extending
  `TextRole` is a `crates/core` and web protocol change.
- **D11 CommonMark edge cases** (`__init__` as strong, `1. 1. 1.` numbered
  1, 2, 3, `*` within words). Recommended: follow CommonMark; code spans
  are the escape hatch.
- **D12 Stock receivers.** Confirm that stock Mumble users keep seeing
  Nuschl Markdown as typed (outbound formatter unchanged by constraint).
