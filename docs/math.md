# Math in messages

People type LaTeX math into messages, `$e^{i\pi} + 1 = 0$` inline and
`$$\int_0^1 x\,dx$$` on its own line, and Nuschl shows it typeset. Nothing
changes on the wire: stock Mumble clients, search, history and copying see
the text as typed, which is what LaTeX users expect from them.

```
message text ──► content worker: the Markdown parse finds formula tokens ──► document
                 (pulldown-cmark, math enabled)                               │ spans and blocks with
                                                                              │ the text as written
desktop: lays out text, asks for each formula at its size ◄────────────────────┘
   │  (LaTeX, inline/display, device pixels per em)
   ▼
math worker (sandboxed, long-lived): LaTeX ─► Typst math ─► restricted Typst ─► coverage mask
   │                                  (own converter, checked)                  + baseline
   ▼
desktop: caches masks, tints them with the text colour, paints them on the baseline
```

## Syntax: one grammar

Formulas are tokens of the Markdown grammar Nuschl renders messages with
([desktop.md](desktop.md#markdown)): pulldown-cmark 0.13, one shared scan for
formulas and the rest of Markdown (`crates/content/src/markdown.rs`;
`math::tokens` is that scan), with no regular expression or other scan of the
text before or after it. The parser decides what is code, an autolink or HTML,
and so where math cannot be; formulas then bind tighter than emphasis and links.

CommonMark ranks code spans, autolinks and raw HTML above every other inline
construct, and Nuschl adds the bare URLs it links (`bare_urls`, the
linkifier's own detection). pulldown-cmark's math (`Options::ENABLE_MATH`) does not quite: a `$`
before a code span can pair with a `$` inside it (``$a `$x$` b`` gives the
formula ``a ` ``, and the code span is gone), which the fuzzer found. So the
text is parsed in CommonMark's order: once without math, where the contents of
code spans, code blocks, inline HTML and bare URLs, and autolinks but their
scheme (so they stay autolinks), are masked byte for byte (line breaks kept, so
every offset stays); then the masked text with math and strikethrough, which
also gives the Markdown structure. The second parse never sees a `$` inside
code, and a token that takes in masked bytes differs from the text and is
refused. A refused token's delimiters are masked and the text is parsed once
more, so what it took in is Markdown again (``$x `y` z$`` shows the code span).

| Construct | Rule |
| --- | --- |
| Inline formula | `$…$`. The opening `$` must be followed, the closing `$` preceded, by a character other than whitespace. Not empty. |
| Display formula | `$$…$$`, anywhere in a paragraph, also over several lines of it. Shown as a block of its own, centred. |
| Math fence | A fenced code block with the language `math` (```` ```math ````), as on GitHub: a display formula. |
| Pairing | An opening delimiter pairs with the first `$` (or `$$`) at the same depth of `{…}` groups. An inline formula cannot contain an unpaired `$`: if that first `$` cannot close, the opening one is a dollar sign. A `$` inside a group belongs to the formula (`$\text{costs $5}$`); an unmatched `}` changes the depth (`$}$` is text). |
| Prices | `$5 and $10` is text: the `$` before 10 follows a space, so it cannot close, and the formula would contain it. |
| Escapes | `\$` is a dollar sign and never a delimiter. Inside a formula nothing is unescaped: the LaTeX is the bytes typed (`$a \$ b \_ \{$` keeps every backslash). |
| Code | Code spans (backticks), fenced and indented code blocks bind tighter: nothing in them is math, `$` included. A formula never contains a backtick. |
| Emphasis | Formulas bind tighter: `*` and `_` inside one are LaTeX (`$*x*$` is a formula). Emphasis around a formula leaves it whole (`*$x$*` is an italic formula). |
| Links | A bare URL (`https://`, `http://`, `mumble://`, as the linkifier finds it) binds tighter, like code: `https://a.example/?q=$x$` is a link, whole. Formulas in a Markdown link's text count (`[$x$](…)`, shown as a formula in the link); none in its destination, in an autolink `<https://…>` or in HTML. |
| Raw HTML | Inline tags are tokens and formulas around them count (`<span>$x$</span>`); an HTML block (a line starting `<div>` and so on, up to a blank line) holds none. |
| Line breaks | An inline formula may continue on the next line of its paragraph; no formula spans a blank line. |
| Lists and quotes | A formula within a line of an item or quote counts. One whose continuation lines carry `>` or indentation does not: its bytes would not be its LaTeX. |
| Empty and unclosed | `$`, `$$`, `$ $`, `$$$$`, `$x` and `$$x` are text. |
| Limits | 32 formulas a message (the rest stay text), 4 KiB of LaTeX each. |

Not supported:

- `\(…\)` and `\[…\]`. CommonMark reads `\(` as an escaped parenthesis, and
  supporting them would mean forking the parser to add a construct with the
  right precedence against code, escapes and links. GitHub has neither.
- GitHub's ``$`…`$`` for formulas that contain dollar signs: the code span binds
  tighter, so it shows as code between dollars. A `$` inside braces or `\$`
  covers most needs.
- Environments outside dollars (`\begin{align}` on its own).

Compared with others (from their documentation and, for pulldown-cmark, its
code; blank where neither says):

| | Nuschl (= pulldown-cmark) | GitHub | Pandoc `tex_math_dollars` | Element (historic) |
| --- | --- | --- | --- | --- |
| Inline | `$…$`, no space inside the delimiters | `$…$`, also ``$`…`$`` | `$…$`, no space inside, no digit after the closing `$` | regular expressions over the Markdown source |
| Display | `$$…$$` within a paragraph, ```` ```math ```` | `$$…$$` as a block, ```` ```math ```` | `$$…$$`, spaces allowed inside | |
| `$5 and $10` | text | | text | math |
| `$5 and x$` | the formula `5 and x` | | the formula `5 and x` | |
| `$x$5` | the formula `x`, then 5 | | text (digit after the closing `$`) | |
| `\$` | dollar sign | dollar sign | dollar sign | eaten by Markdown escaping |
| Code | code binds tighter | | code binds tighter | math inside code |
| `_`, `*` inside | LaTeX | | LaTeX | emphasis |
| `\(…\)` | no | no | `tex_math_single_backslash` | |

Element's problems came from finding math with regular expressions before and
after Markdown: math in code, emphasis and escapes inside formulas, prices as
math, formulas across paragraphs, and other clients seeing something else than
was typed. Each of those cases is a row of the spec test in
`crates/content/src/math.rs` (input → expected tokens), and the `math_tokens`
fuzz target checks the invariants for any input: every token is exactly the
bytes between its delimiters, none overlaps what is code when the same text is
parsed without math or a URL the linkifier links, the document shows the tokens in order as written, and
the document's text, formulas included, is the message's with exactly the
Markdown syntax it hides left out (`markdown::check`, also run by the
`markdown` fuzz target and the property test over generated messages).

### Messages from stock clients (HTML)

Stock Mumble sends HTML. Its HTML becomes inert text with markers for styles
(the existing `legacy_text`), and that text goes through the same parse; there
is no math rule in the HTML tokenizer. A formula counts only within one run of
visible text: it may not cross an element's boundary (`$x<b>y</b>$` is text),
and `<code>`, `<tt>`, `<kbd>`, `<samp>`, `<pre>` (a code block) and `<a>` hold
none. HTML-escaped text (`$a &lt; b$`) arrives as typed (`a < b`).

## Converting and rendering

The math worker converts LaTeX to Typst math markup with its own converter
(`crates/math/src/latex.rs`) and lays it out with the `typst` crate, then
rasterises with `typst-render`.

**Why not MiTeX.** MiTeX (Apache-2.0) covers more of LaTeX, but its output is
Typst *code*: it writes `#…` calls for many commands, passes raw Typst through
from `\iftypst … \fi`, and relies on its Typst package's scope (with another
package, xarrow) at evaluation. Its build script runs a `typst` binary from
`PATH` if one is there, and it depends on `fxhash` (unmaintained) and rkyv 0.7.
Our converter writes only math markup, so the property that matters is simple
and checked:

- Letters are written one at a time with spaces (`ab` → `a b`), so they never
  form an identifier and cannot name a function; punctuation is spaced or
  escaped (`\(`, `\,`, `\#`, `\$`), so it never forms a shorthand, a code
  expression or the end of the equation; `\text{…}` becomes a string literal.
- Commands map to Unicode symbols (`\le` → `≤`, `\sum` → `∑`, which Typst lays
  out by math class) or to a fixed list of Typst math functions (`frac`, `sqrt`,
  `root`, `mat`, `cases`, `lr`, accents, fonts, `op`, `limits`, `class`, …).
- Every result is parsed again as Typst math and refused unless it contains no
  code (except the literal `#none` of a matrix without delimiters), no `$`, no
  syntax error and only identifiers from that list (`latex::check`). The fuzz
  target `math` asserts that the converter never writes what the check refuses.

Supported: about 330 named symbols (Greek and Hebrew letters, operators,
relations, arrows, big operators, dots and delimiters); sub- and superscripts
and primes;
`\frac` and its variants, `\binom`, `\sqrt[n]`; `\left … \middle … \right`;
accents; `\mathbf`, `\mathbb`, `\mathcal`, `\mathfrak`, `\mathscr`, `\mathrm`,
`\mathsf`, `\mathtt`, `\boldsymbol` and the old `{\bf …}`; `\text` and its
variants (with `$…$` inside); `\operatorname(*)`, the named functions (`\sin`,
`\lim`, …); `\overset`, `\underset`, `\stackrel`, braces with annotations,
`\xrightarrow`; `\not`; spaces; `\pmod`; the matrix, cases and alignment
environments (`matrix`, `pmatrix`, `bmatrix`, `Bmatrix`, `vmatrix`, `Vmatrix`,
`smallmatrix`, `array`, `cases`, `dcases`, `aligned`, `align`, `gather`,
`split`, `equation`, …). `\color` is accepted and ignored (formulas take the
text colour). Unknown commands are an error with a reason.

A native Typst syntax (say ```` ```typst-math ````) is not offered: users know
LaTeX, and Typst math can call any function in the math scope, so it would
need its own allow list. The converter's check is that list, should it come.

### Isolation and limits

Typst is a scripting language with file access, packages, data loaders, WASM
plugins and `eval`. Three walls keep formulas from reaching any of it:

1. The converter writes no code, and its output is checked (above).
2. Typst runs in a `World` with one file, the formula; every other file and
   package is access denied, there is no clock (`today` is `None`) and no system
   font. Its standard library is rebuilt without `read`, `plugin`, `image`,
   `json`, `yaml`, `toml`, `csv`, `cbor`, `xml`, `bibliography`, `cite`, `eval`,
   `sys`, `pdf`, `html` and `std`; a test compiles `#read(…)` and
   `#std.read(…)` and gets errors.
3. The worker is a separate process in the content worker's sandbox: bwrap with
   all namespaces unshared (no network), read-only runtime libraries and itself,
   an empty environment, private `/tmp`; on macOS the content Seatbelt profile;
   on Windows what the content worker gets (see SECURITY.md).

Bounds: 4 KiB of LaTeX and 64 KiB of Typst a formula; 32 levels of groups,
arguments, environments and `\left`; 400 matrix cells; images of at most 4096
× 1024 device pixels and 2 Mpixel (larger formulas are refused before they are
rasterised); sizes from 4 to 128 device pixels per em; a three-second deadline
per formula, start-up included, after which the worker is killed; 512 MiB of
address space and a lifetime CPU limit of 120 s (it is restarted on demand);
Typst's thread pool has one thread; memoized results are evicted after each
formula; 32 formulas a message; the desktop queues at most 64 formulas and
caches 512.

The `math` fuzz target runs hostile LaTeX through the converter, the check,
Typst and the rasteriser; `math_tokens` checks the grammar (above). They found
code spans losing to math in pulldown-cmark (above), and in the converter a
line break escaping the next parenthesis, `\not` making identifiers and
panicking before a control character, and delimiter and root characters
(`⟨`, `√`) taking in the parenthesis that closes a call; each is fixed with a
regression test. Open: Typst's own debug assertion that shaped glyphs stay in
their text's range fails for some right-to-left text in `\text{…}`
(`typst-layout`, `inline/shaping.rs`). Release builds have no such check and
render it; a debug build's worker panics, is replaced, and the formula shows
as its source. It also stops `just fuzz math` (debug assertions) on such
inputs.

### Dependencies

`typst` 0.15 and `typst-render` pull in about 270 crates, all Rust and all with
licences `cargo deny` accepts. Exceptions: `psm` (via `stacker`, which Typst's
evaluator uses to grow its stack on deep recursion) is a few lines of assembly
per platform; `deny.toml` confines it to `typst-eval`. Advisories in Typst's
dependency tree concern its citation styles (quick-xml) and YAML loaders, which
the restricted library removes; they are ignored with reasons. `deny.toml`
confines `typst`, `typst-layout` and `typst-render` to `nuschl-math`, and that
to `nuschl-math-worker`.

The one font is New Computer Modern Math (`crates/math/fonts`), Typst's default
math font, embedded unmodified: GUST Font License (LPPL 1.3c), not OFL. Its text
faces are not shipped (NewCM10-Regular is GPL-3.0 with a font exception);
`\text` uses the math font's upright Latin letters, so other scripts in `\text`
show as missing glyphs.

## Output

The worker returns a coverage mask (one byte a pixel) at the size the formula is
shown in device pixels, and the baseline. The desktop tints it with the text
colour where it paints it, so light and dark themes, quotes and headings need no
new rendering; a zoom or scale change asks for the new size (the old one stands
in meanwhile) and the result is sharp at every zoom. Vector paths would be
sharp too, but the interface would have to tessellate untrusted geometry; a
mask is bounded data it already knows how to draw. Masks are cached by (LaTeX,
inline or display, size), tinted images by colour.

Formulas are set at 1.2 em (Computer Modern looks small beside interface fonts;
KaTeX uses 1.21). An inline formula stands in the laid-out text as no-break
spaces as wide as it is (so it wraps as a word) and is painted over them on the
text's baseline; the line height grows for a tall one. Carets, the selection,
links and copying keep the text's own offsets, so selecting across a formula
and copying gives its source, and "Copy message" gives the message as typed
(a display formula written within a line stays within it).

Screen readers get each formula as a math node whose name is its source
(`$\pi r^2$`). Until a formula is ready its source shows faintly; one that
cannot be rendered shows as code with a wavy underline (inline) or in a code
box with the reason (display), never as a blank.

## Footprint

Measured on x86_64 Linux, release profile (thin LTO, one codegen unit):

| | Math worker | Content worker, for comparison |
| --- | --- | --- |
| Binary (as built, symbols kept) | 43.1 MB | 7.1 MB |
| Binary, stripped | 36.4 MB | 5.4 MB |
| Start in the sandbox and first formula | 10–11 ms | |
| Each further display formula | 0.2–0.3 ms | |
| Resident memory, started / after 205 formulas (one a 20 × 20 matrix) | 11 MB / 22 MB | |

Most of the size is Typst's library: its embedded data (syntax definitions,
hyphenation patterns, citation styles, PDF fonts) and the code that uses it,
none of which formulas need; the math font is 1.3 MB. The desktop binary does
not grow: it links no Typst. (`cargo test --release -p nuschl-math-worker
--test worker -- --ignored --nocapture cold_start` measures start-up and cost.)

The worker starts on the first formula and stops after a minute without one.

## Setting

Settings → Appearance → **Render math**, on by default. Off, formulas show as
the text that was typed.
