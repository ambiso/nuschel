#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Exact legacy domain/tag welcome report, private stock Murmur/native only."""

import argparse
import json
from pathlib import Path

import document_link_regression as fixture
import welcome_html
from accessibility import items
from local_murmur import eventually
from workers import exchange

SAMPLES = [
    "<b>something.de</b>",
    "<strong>something.de</strong>",
    "<b>https://something.de</b>",
    "<a href='https://something.de'><b>something.de</b></a>",
    "&lt;b&gt;literal.test&lt;/b&gt;",
    "first<br/><br/>second<br/>```rust<br/>let host = &quot;code.test&quot;;<br/>```",
]
EXTRA = (
    "<br/>Domain emphasis: <b>something.de</b>"
    "<br/>Linked emphasis: <a href='https://linked.test/'><b>linked.test</b></a>"
    "<br/>Literal: &lt;em&gt;literal.test&lt;/em&gt;"
    "<br/>```rust<br/>let host = &quot;code.test&quot;;<br/>```"
)


def actions(args, root, shell, application, bot, murmur):
    def visible():
        return "\n".join(name for _, _, name, _ in items(application))

    text = eventually(
        lambda: (
            value if "Domain emphasis: something.de" in (value := visible()) else None
        ),
        "exact emphasized domain missing from native welcome",
    )
    assert "bsomething.de/b" not in text
    assert "<b>something.de</b>" not in text
    assert "linked.test" in text
    assert "<em>literal.test</em>" in text
    assert "code.test" in text
    assert "hiddenWelcome()" not in text
    shell.screenshot(args.output / "domain-welcome.png")
    (args.output / "domain-visible.txt").write_text(text)
    return {
        "passed": True,
        "exact_domain_visible": True,
        "marker_leak_absent": True,
        "literal_tags_preserved": True,
        "code_visible": True,
        "private_stock_murmur": True,
        "public_urls_activated": False,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binaries", "murmur", "at-spi", "pulse-bin", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    prepared = exchange(
        args.binaries / "nuschl-content-worker",
        [{"kind": "legacy_html", "html": source} for source in SAMPLES],
    )
    for source, result in zip(SAMPLES[:4], prepared[:4]):
        assert result["status"] == "prepared", source
        block = result["value"]["document"]["blocks"][0]
        expected = "https://something.de" if "<b>https:" in source else "something.de"
        assert block["text"] == expected, result
        assert any(
            span["style"]["bold"]
            and span["start"] == 0
            and span["end"] == len(expected)
            for span in block["spans"]
        ), result
    (args.output / "domain-worker.json").write_text(
        json.dumps(list(zip(SAMPLES, prepared)), indent=2) + "\n"
    )
    args.document_links, args.expect_markers = True, False
    fixture.WELCOME_EXTRA, fixture.run_actions = EXTRA, actions
    welcome_html.run(args)
    proof = json.loads((args.output / "proof.json").read_text())
    proof["welcome_domain"] = proof.pop("document_links")
    proof["exact_worker_domain_styles"] = True
    (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")


if __name__ == "__main__":
    main()
