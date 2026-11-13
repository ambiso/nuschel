# Emoji

Nuschl draws emoji with [Twemoji](https://github.com/jdecked/twemoji)'s images in
the desktop and in the web client, and both have an emoji picker beside the
composer. Names, groups, keywords and skin-tone variants come from
[emojibase-data](https://www.npmjs.com/package/emojibase-data). Before, the desktop
drew emoji with whichever font shaped them, so the UI font's black-and-white
glyphs won over the colour emoji font where it had any: DejaVu Sans has the
classic smileys ([#7](https://github.com/ambiso/nuschl/issues/7)).

## The data

Both sources are flake inputs with `flake = false`, pinned in `flake.lock`:

| Input | Pinned | Licence | Used for |
| --- | --- | --- | --- |
| `twemoji` | `github:jdecked/twemoji/v17.0.3` | graphics CC-BY 4.0 | the images (`assets/svg`) |
| `emojibase-data` | `emojibase-data-17.0.0.tgz` from npm | MIT | groups, names, keywords, short codes, skin tones |

`nix/emoji.nix` runs `nix/emoji.py` over them (`nix build .#emojiAssets`). Callers
outside the flake (`nix/crate2nix.nix`, the distro packaging) read the same pins
from `flake.lock`, so every build path uses identical store paths. The output:

- `emoji.txt`: the picker's table. One `@Group` line per group, then one emoji per
  line: the emoji, its name, its search words (name, keywords and emojibase short
  codes) and, if it has them, its five skin-tone variants (light to dark). People
  of two skin tones are offered with the same tone for both, as platform pickers
  do. The components group (skin tones, hair styles) and lone regional indicators
  are left out: 1914 emoji in 9 groups.
- `images.txt`: every image's key, marking the characters that are text by default.
- `data.json`: the table and the keys for the web client.
- `svg/<key>.svg`: all 4009 Twemoji images, renamed by key; the drawings are
  unchanged. A key is the code points in lower-case hex without leading zeros,
  joined by `-`, without U+FE0F (`2122`, `1f44d-1f3fd`, `23-20e3`).
- `LICENSE-GRAPHICS`, `LICENSE-emojibase` and `ATTRIBUTION.txt`, the notices. They
  are the `emoji-assets` component of the release notices (docs/release-notices.md),
  and both pickers credit Twemoji in their footer.

emojibase-data is published under MIT, but its English names and keywords derive
from Unicode CLDR's annotations, which are under the Unicode License v3. The pinned
package carries no Unicode notice, and the bundle does not add one: whether one is
needed is an open question for the release licence review.

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

`build.rs` embeds the table and the images (about 10 MB of SVG in the binary). It
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
category (leaving the search); the tab of the category in view is selected. The grid
has a scrollbar, like the channel list. The hand beside the search chooses the skin
tone, which applies to every emoji that has tones. A click or Enter inserts at the
composer's cursor, replacing the selection, and closes the picker; with Shift held
it stays open for more. Escape closes it. Recently used emoji come first. The skin
tone and the recent emoji last until the desktop quits: settings.json cannot gain
keys without breaking the 0.1.0 release that reads it (`RELEASED_KEYS` in
`crates/session/src/config.rs`).

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
  emoji last as long as the page. Not yet on the web: the category tabs and
  searching by an emoji, which so far only the desktop has.

## Tests

- `cargo test -p nuschl` (`emoji::tests`, crate2nix: `checks.<system>.desktop-unit`):
  the table and its tones, search (also by an emoji), the category tabs, the presentation rule (`™` and `™️`, ©, ®, ❤, ☝,
  U+FE0E, sequences, flags, keycaps, plain text), the images' shape, and that an
  image renders at the requested size.
- `node --test src/emoji.test.js` in `web/app`, run by the web build: the same
  rules on the generated `data.json`, refusing malformed data, loading once, and
  inserting at the cursor.
- `tests/emoji_smoke.py` (tests/README.md): the desktop in a private headless GNOME
  Shell. It checks by pixel colours that `™` paints no Twemoji and `™️` does, that
  😀 is Twemoji's yellow, that the picker's search, skin tone, Shift+Enter,
  Shift+click and click insert the expected text into the composer, which shows
  them as Twemoji, and that the sent message is that text. Then the search field:
  that it shows a cursor, that a click on a result inserts it (also in the last
  column), that Ctrl+V pastes and Ctrl+Backspace deletes a word; that the grid has
  a scrollbar; and that the Flags tab jumps to 🏁. `--no-category-row` runs these
  against a build from before the category tabs, to see them fail there.

The GPUI patch itself has no test in its crate, whose test target does not build
here (`vendor/gpui-pre/NUSCHEL-PATCH.md`); the smoke test checks what it paints.
The web picker has no browser test; its logic is in `emoji.js`, which is tested.

## Updating

Change the `twemoji` or `emojibase-data` URL in `flake.nix`, then run
`nix flake lock` and `nix build .#emojiAssets`. Its output says how many emoji
have no image, which should stay none. Then run both clients' tests; the
`emoji-assets` notice picks up the new pins (docs/release-notices.md).
