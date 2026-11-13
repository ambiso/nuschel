#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Build the emoji assets the desktop and web clients compile in or serve.

    python3 -I nix/emoji.py --twemoji <jdecked/twemoji> --emojibase <emojibase-data> --output <dir>

Both inputs are pinned flake inputs (see nix/emoji.nix); this only reads them.

Output:
  emoji.txt   The picker's table: `@Group` lines, then one emoji per line as
              tab-separated fields: emoji, name, search words, short codes
              (emojibase's, space-separated, the first the main one: `heart
              red_heart` for :heart:), and either no more fields or five
              skin-tone variants (light to dark; empty where Unicode has none
              for that tone).
  images.txt  Every image key, one per line; a key followed by a tab and "text"
              is a character that Unicode shows as text by default (™, ©, ❤):
              clients draw it as an image only when U+FE0F follows it, and
              never when U+FE0E follows it.
  data.json   The table and the image keys for the web client.
  svg/<key>.svg
              Every Twemoji image, named by its key: the code points in
              lower-case hex without leading zeros, joined by "-", without
              U+FE0F. Clients compute the same key for text they render.
  LICENSE-GRAPHICS, LICENSE-emojibase, ATTRIBUTION.txt
              Twemoji's CC-BY 4.0 grant, emojibase-data's MIT licence and the
              attribution the graphics licence requires.

The SVGs are checked to be small, static drawings: no scripts, embedded or
linked resources, or styles, so neither client ever fetches or runs anything
an image names.
"""
import argparse
import json
import re
import shutil
from pathlib import Path

MAX_SVG = 64 * 1024
# Anything that could reference another resource or run code (a fragment
# `url(#id)` names a clip path in the same drawing). Twemoji's
# drawings are plain paths, circles and ellipses with fill colours.
FORBIDDEN = re.compile(
    rb"<script|<image|<use|<foreignObject|<style|<a[\s>]|href|url\((?!#[A-Za-z0-9_-]+\))|@import|on[a-z]+=|<!ENTITY|<!DOCTYPE",
    re.IGNORECASE,
)
TONES = 5
# A short code as typed between colons; emojibase's are unique and at most 46
# characters long.
SHORTCODE = re.compile(r"[a-z0-9_+-]{1,64}")


def key(hexcode):
    """The image key of an emojibase hexcode such as `00A9-FE0F`."""
    points = [int(part, 16) for part in hexcode.split("-")]
    return "-".join(format(point, "x") for point in points if point != 0xFE0F)


def words(*texts):
    seen = []
    for text in texts:
        for word in re.split(r"[^\w']+", text.lower()):
            if word and word not in seen:
                seen.append(word)
    return seen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--twemoji", required=True, type=Path)
    parser.add_argument("--emojibase", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    svg_out = args.output / "svg"
    svg_out.mkdir(parents=True)
    images = set()
    for path in sorted((args.twemoji / "assets/svg").iterdir()):
        if path.suffix != ".svg" or not path.is_file() or path.is_symlink():
            raise SystemExit(f"unexpected Twemoji asset {path.name}")
        data = path.read_bytes()
        if len(data) > MAX_SVG or not data.lstrip().startswith(b"<svg") or FORBIDDEN.search(data):
            raise SystemExit(f"Twemoji asset {path.name} is not a small static drawing")
        name = key(path.stem)
        if name in images:
            raise SystemExit(f"two Twemoji assets share the key {name}")
        images.add(name)
        (svg_out / f"{name}.svg").write_bytes(data)

    base = args.emojibase / "en"
    data = json.loads((base / "data.json").read_text())
    messages = json.loads((base / "messages.json").read_text())
    shortcodes = json.loads((base / "shortcodes/emojibase.json").read_text())
    groups = {group["order"]: group["message"] for group in messages["groups"]}

    def aliases(hexcode):
        found = shortcodes.get(key(hexcode).upper(), shortcodes.get(hexcode, []))
        return [found] if isinstance(found, str) else found

    # Default text presentation: a single code point, which emojibase gives
    # type 0 (its `emoji` field then carries U+FE0F, its `text` U+FE0E).
    text_default = set()
    for emoji in data:
        for entry in [emoji, *emoji.get("skins", [])]:
            if entry.get("type") == 0:
                name = key(entry["hexcode"])
                if "-" in name:
                    raise SystemExit(f"text-presentation sequence {entry['hexcode']}")
                text_default.add(name)

    table = {}
    skipped = []
    codes = set()
    for emoji in sorted(data, key=lambda emoji: emoji.get("order", 1 << 30)):
        group = emoji.get("group")
        # Group 2 holds the components (skin tones, hair styles); emoji without
        # a group are the regional indicators.
        if group is None or group == 2:
            continue
        if key(emoji["hexcode"]) not in images:
            skipped.append(emoji["emoji"])
            continue
        tones = [""] * TONES
        for skin in emoji.get("skins", []):
            tone = skin.get("tone")
            if isinstance(tone, list):
                # People of two skin tones: the picker offers each tone for
                # both, as the platform pickers do.
                if len(set(tone)) != 1:
                    continue
                tone = tone[0]
            if isinstance(tone, int) and 1 <= tone <= TONES and key(skin["hexcode"]) in images:
                tones[tone - 1] = skin["emoji"]
        codes_of = aliases(emoji["hexcode"])
        for code in codes_of:
            if not SHORTCODE.fullmatch(code) or code in codes:
                raise SystemExit(f"unexpected or repeated short code {code!r} for {emoji['hexcode']}")
            codes.add(code)
        search = words(emoji["label"], " ".join(emoji.get("tags", [])), " ".join(codes_of))
        for field in [emoji["emoji"], emoji["label"], *tones]:
            if "\t" in field or "\n" in field:
                raise SystemExit(f"unexpected separator in {emoji['hexcode']}")
        name = emoji["label"]
        table.setdefault(group, []).append(
            [emoji["emoji"], name, " ".join(search), " ".join(codes_of), *([] if not any(tones) else tones)]
        )

    lines = []
    web = {"groups": [], "images": sorted(images), "text": sorted(text_default & images)}
    for order in sorted(table):
        title = groups[order][:1].upper() + groups[order][1:]
        lines.append("@" + title)
        lines.extend("\t".join(row) for row in table[order])
        web["groups"].append({"name": title, "emoji": table[order]})
    (args.output / "emoji.txt").write_text("\n".join(lines) + "\n")
    (args.output / "images.txt").write_text(
        "".join(f"{name}\ttext\n" if name in text_default else f"{name}\n" for name in sorted(images))
    )
    (args.output / "data.json").write_text(json.dumps(web, ensure_ascii=False, separators=(",", ":")))

    shutil.copyfile(args.twemoji / "LICENSE-GRAPHICS", args.output / "LICENSE-GRAPHICS")
    shutil.copyfile(args.emojibase / "LICENSE", args.output / "LICENSE-emojibase")
    (args.output / "ATTRIBUTION.txt").write_text(
        "Emoji graphics: Twemoji, Copyright 2019 Twitter, Inc and other contributors,\n"
        "and the jdecked/twemoji maintainers; licensed under CC-BY 4.0 (LICENSE-GRAPHICS),\n"
        f"https://github.com/jdecked/twemoji/tree/{args.revision} (version {args.version}).\n"
        "The images are renamed by code point; their drawings are unchanged.\n\n"
        "Emoji names, groups and keywords: emojibase-data 17.0.0 (MIT, LICENSE-emojibase),\n"
        "https://www.npmjs.com/package/emojibase-data.\n"
    )
    count = sum(len(rows) for rows in table.values())
    print(f"{count} emoji in {len(table)} groups, {len(images)} images; without an image: {' '.join(skipped) or 'none'}")


if __name__ == "__main__":
    main()
