# Emoji

Nuschl draws emoji with [Twemoji](https://github.com/jdecked/twemoji)'s images in
the desktop and in the web client, and both have an emoji picker beside the
composer. Names, groups, keywords and skin-tone variants come from
[emojibase-data](https://www.npmjs.com/package/emojibase-data). Before, the desktop
drew emoji with whichever font shaped them, so the UI font's black-and-white
glyphs won over the colour emoji font where it had any: DejaVu Sans has the
classic smileys ([#7](https://github.com/ambiso/nuschl/issues/7)).

## The data

The three sources are pinned together, by hash, in `nix/emoji.nix`. They belong
together: Twemoji draws the emoji of one Unicode Emoji version, emojibase-data
names and groups that version's emoji from one CLDR release, and that release's
licence covers the names and keywords.

| Source | Pinned | Licence | Used for |
| --- | --- | --- | --- |
| Twemoji | `jdecked/twemoji` v17.0.3 (commit `b6b55fef`) | graphics CC-BY 4.0 | the images (`assets/svg`) |
| emojibase-data | `emojibase-data-17.0.0.tgz` from npm (Emoji 17, CLDR 48) | MIT | groups, names, keywords, short codes, skin tones |
| CLDR licence | `LICENSE` of `unicode-org/cldr` release-48 (commit `acd6d88a`) | Unicode License v3 | the notice for the names and keywords |

`nix/emoji.nix` runs `nix/emoji.py` over them (`nix build .#emojiAssets`). Every
caller (the flake, `nix/crate2nix.nix`, the distro packaging) imports that file,
so every build path uses identical store paths. The output:

- `emoji.txt`: the picker's table. One `@Group` line per group, then one emoji per
  line: the emoji, its name, its search words (name, keywords and emojibase short
  codes), its short codes (`heart red_heart` for ❤️, the main one first) and, if it
  has them, its five skin-tone variants (light to dark). The generator refuses a
  short code outside `[a-z0-9_+-]{1,64}` or one used twice; every emoji has one. People
  of two skin tones are offered with the same tone for both, as platform pickers
  do. The components group (skin tones, hair styles) and lone regional indicators
  are left out: 1914 emoji in 9 groups.
- `images.txt`: every image's key, marking the characters that are text by default.
- `data.json`: the table and the keys for the web client.
- `svg/<key>.svg`: all 4009 Twemoji images, renamed by key; the drawings are
  unchanged. A key is the code points in lower-case hex without leading zeros,
  joined by `-`, without U+FE0F (`2122`, `1f44d-1f3fd`, `23-20e3`). The web
  client serves these.
- `svgz/<key>.svgz`: the same images gzip-compressed by zopfli, for the desktop:
  4.2 MB instead of 10 MB. `nix/emoji.nix` makes them after the checks and fails
  unless each decompresses to exactly its SVG; zopfli's output has no file name
  and no time, so it is reproducible.
- `LICENSE-GRAPHICS`, `LICENSE-emojibase`, `LICENSE-unicode-cldr` and
  `ATTRIBUTION.txt`, the notices. They are the `emoji-assets` component of the
  release notices (docs/release-notices.md), and both pickers credit Twemoji in
  their footer.

emojibase-data is published under MIT, but its English names and keywords derive
from Unicode CLDR's annotations, which are under the Unicode License v3. The npm
package carries no Unicode notice (nor does emojibase's repository), so the bundle
takes the licence from the CLDR release emojibase-data was built from, and the
attribution names it. The generator checks that it is the Unicode License v3.

The generator refuses an image larger than 64 KiB or one that could run code or
load anything: scripts, `<image>`, `<use>`, `<foreignObject>`, styles, links,
`href`, non-fragment `url(...)`, entities. Twemoji's drawings are plain paths, with
three clip paths referenced by fragment. Every emoji of the table must have an
image in every tone it offers (tested in both clients).

## Which text is an image

Both clients decide from the text alone, never from the font that shaped it, with
the same rule (`find_images` in `apps/desktop/src/emoji.rs`, `findImages` in
`web/app/src/emoji.js`):

- At each position, the longest code-point sequence that has an image is one
  image. U+FE0F is ignored when matching but belongs to the image, so `❤️‍🔥`,
  `🧑🏻‍🤝‍🧑🏿`, flags (`🇩🇪`) and keycaps (`1️⃣`) are each one image.
- A single character that Unicode shows as text by default (emojibase's type 0:
  ™ © ® ❤ ☝ and 202 more) is an image only with U+FE0F after it. `™` stays
  text; `™️` is Twemoji's ™.
- Anything followed by U+FE0E stays text, even 😀.
- Everything else stays text: letters, digits, `#`, `*`, arrows and other symbols.

The picker inserts emojibase's emoji form, so a ™ from the picker is `™️`.

## Desktop

`build.rs` embeds the table and the compressed images (about 4.2 MB in the
binary). GPUI's SVG parser (usvg) recognises gzip and decompresses an image when
it is first drawn, with `flate2`, which the desktop already uses; nothing new
is linked. It
reads them from `NUSCHL_EMOJI_ASSETS`, which every Nix build sets, or else from
`emoji-assets/` in the workspace root (`just emoji-assets`; the Debian source
package ships it). It fails without them rather than build a desktop without emoji.

Drawing is a patch to the vendored GPUI (`vendor/gpui-pre/NUSCHEL-PATCH.md`): the
desktop registers its Twemoji at startup, and GPUI asks it for the emoji ranges of
every line it shapes and paints. That covers messages, the composer and all other
text, such as names and channel descriptions. The ranges are shaped with the
system's colour emoji font (Noto Color Emoji, Segoe UI Emoji or Apple Color Emoji),
so they get an emoji's width even where the text's font has a glyph of its own.
Each range is then painted as one image over that advance. Selection, the cursor,
copying and screen readers keep the text. Images are rasterised with GPUI's SVG
renderer, like the interface icons, on first paint at each size; the cache holds
at most 4096 sizes and images. Beyond that, at edges above 512 device pixels, or
if an image does not render, the font's glyphs are painted.

The picker opens with the smiley button beside the composer, or with Ctrl+. or
Ctrl+; as in GTK. It has three tabs: Emoji, and Stickers and GIFs, which are
placeholders for now. The search is an ordinary text field (cursor, selection,
clipboard, Ctrl+Backspace). Typing searches names, keywords and short codes; each
word must start a word of one of them. An emoji in the search, typed or pasted,
with or without U+FE0F or a skin tone, finds itself first, then every emoji sharing
any of its words, those sharing most first: ❤️ lists the other hearts. Other words
beside it narrow that list. Only the first 64 characters are searched. Under the
search, a tab per category of the table, drawn with its first emoji, jumps to that
category (leaving the search); the tab of the category in view is underlined. The
emoji to click are on the message area's colour (white in the light theme), below a
line that separates them from the tabs, and have the app's shared scrollbar
(`layout::scroll_frame`, in a gutter right of the emoji), as the other lists do.
Each cell's element id is its place in the grid, not the emoji: an emoji shown twice
(recently used and in its group) would otherwise share hover, tooltip and click
state, and neither would show its tooltip or take a click. The hand beside the search chooses the skin
tone, which applies to every emoji that has tones. A click or Enter inserts at the
composer's cursor, replacing the selection, and closes the picker; with Shift held
it stays open for more. Escape closes it. Recently used emoji come first. The skin
tone and the recent emoji last until the desktop quits: settings.json cannot gain
keys without breaking the 0.1.0 release that reads it (`RELEASED_KEYS` in
`crates/session/src/config.rs`).

Short codes type emoji in the composer (`emoji/shortcodes.rs`), as in chat apps:
`:heart:` becomes ❤️. A colon and two characters of a code list up to eight emoji
above the composer, each with the code that matched and its name: the code itself
first, then codes starting with what is typed, then codes with a later word
starting with it (`:smile` also finds `:sweat_smile:`), shorter codes first. Up and
Down choose, Enter or Tab types the chosen one, a click types that one, and Escape
closes the list until another code is started. Typing the closing colon of a known
code replaces it at once; a pasted code, or one brought back by Ctrl+Z, stays text.
The emoji takes the picker's skin tone and joins the recently used. A code starts
only at the start of the text, after a space, a bracket, a quote or another emoji,
never inside a word, a time (`10:30`) or an address (`https://`). Nothing happens
while text is selected or an input method is composing.

Emoji in messages (`emoji/info.rs`): over an emoji, the pointer becomes a hand and
a tooltip names it (with its skin tone: "waving hand: medium skin tone"), as the
picker's tooltips do. A click, without dragging, opens a card beside it with the
emoji at 64 pixels, its name and, beside "Type", every short code that types it,
one per row, the main one first; a click outside or Escape closes it. The emoji under the pointer is found
with the same rule that draws it, in the line under the pointer, so what is named
is what is shown; links keep their own hover and click, and selecting text shows
no tooltip. Only emoji of the table are named: a lone skin tone or regional
indicator is drawn but has no name.

## Web

The web build copies the assets to `/app/emoji/` (`nix/web-client.nix`). The page
loads `data.json` once from its own origin and validates it before use: the shape,
the keys' syntax, field lengths, at most 2 MiB, 32 groups, 8192 emoji and 16384
images. Images are `<img>` elements on the same origin, named only by a key from
that index (the existing CSP's `img-src 'self'`). Until the data arrives, or if it
cannot be loaded, emoji stay text and the picker says so.

- Messages: text and links show Twemoji images; inline code and code blocks keep
  their characters. An emoji stays text in the page, transparent over its image
  (a CSS background, set through the CSSOM, which the CSP allows), so selecting,
  copying, find-in-page and screen readers get the characters in every browser;
  an `<img alt>` is left out of copied text by some.
- The composer stays a plain `<textarea>` for accessibility: what is typed shows
  in the browser's own emoji font.
- The picker matches the desktop's: the same tabs, search, skin tone, click,
  Shift+click and Enter, with Ctrl+. or Ctrl+; from the composer, and Escape or a
  click outside to close it. The skin tone is kept in `localStorage`; the recent
  emoji last as long as the page. Not yet on the web: the category tabs,
  searching by an emoji, short codes in the composer and the emoji card, which so
  far only the desktop has. The web client validates the short-code field.

## Tests

- `cargo test -p nuschl` (`emoji::tests`, crate2nix: `checks.<system>.desktop-unit`):
  the table and its tones, search (also by an emoji), the category tabs, short codes
  (where a code may start, closing colons, ranking, typed versus pasted colons),
  naming the emoji at an offset in a message, the presentation rule (`™` and `™️`, ©, ®, ❤, ☝,
  U+FE0E, sequences, flags, keycaps, plain text), the images' shape, and that an
  image renders at the requested size.
- `node --test src/emoji.test.js` in `web/app`, run by the web build: the same
  rules on the generated `data.json`, refusing malformed data, loading once, and
  inserting at the cursor.
- `tests/emoji_smoke.py` (tests/README.md): the desktop in a private headless GNOME
  Shell. It checks by pixel colours that `™` paints no Twemoji and `™️` does, that
  😀 is Twemoji's yellow, that the picker's search, skin tone, Shift+Enter,
  Shift+click and click insert the expected text into the composer, which shows
  them as Twemoji, and that the sent message is that text. Before that, with nothing
  picked yet, it Shift+clicks every emoji the picker shows whole when it opens and
  expects exactly those, in the table's order (`--emoji-table`, the table of
  `nix build .#emojiAssets`; by default `emoji-assets/emoji.txt`), then clicks an
  emoji shown both under Recently used and in its group (tooltip and insert). Then
  the search field:
  that it shows a cursor, that a click on a result inserts it (also in the last
  column), that Ctrl+V pastes and Ctrl+Backspace deletes a word; that the grid has
  a scrollbar; and that the Flags tab jumps to 🏁 and is underlined. `--no-category-row` runs these
  against a build from before the category tabs, to see them fail there. Then short
  codes: `:hea` and Enter, a typed `:fire:`, `:thu`, Down and Tab (in the chosen
  skin tone), Escape keeping `:smile` as text, and Ctrl+Z after `:fire:` keeping the
  code, each checked by what is sent; and Ada's 🔥 in a message: hovering it shows a
  tooltip, a click shows its larger card, and Escape closes it.

The GPUI patch itself has no test in its crate, whose test target does not build
here (`vendor/gpui-pre/NUSCHEL-PATCH.md`); the smoke test checks what it paints.
The web picker has no browser test; its logic is in `emoji.js`, which is tested.

## Updating

Update the three pins in `nix/emoji.nix` together: a Twemoji release, the
emojibase-data release for the same Emoji version, and the CLDR release that
emojibase-data's changelog names (its `release-NN` tag). Set each `hash` to `""`,
run `nix build .#emojiAssets`, and take the hash Nix reports, one at a time. Its
output says how many emoji have no image, which should stay none. Then run both clients' tests; the
`emoji-assets` notice picks up the new pins (docs/release-notices.md).
