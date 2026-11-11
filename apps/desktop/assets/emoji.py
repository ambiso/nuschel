#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Generate emoji.txt (the emoji picker's table) from Unicode's emoji-test.txt.

    python3 apps/desktop/assets/emoji.py path/to/emoji-test.txt > apps/desktop/assets/emoji.txt

Keeps fully qualified emoji without skin-tone variants and the Component
group; one `@Group` line per group, then `<emoji> <name>` lines.
"""
import sys

def main(path):
    group = None
    for line in open(path, encoding="utf-8"):
        if line.startswith("# group: "):
            group = line[len("# group: "):].strip()
            if group != "Component":
                print("@" + group)
        elif group != "Component" and "; fully-qualified" in line:
            emoji, name = line.split("# ", 1)[1].strip().split(" ", 1)
            name = name.split(" ", 1)[1]  # drop the "E15.0" version
            if "skin tone" not in name:
                print(emoji, name)

if __name__ == "__main__":
    main(sys.argv[1])
