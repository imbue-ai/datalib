"""Write slack_shortcodes.tsv from iamcal/emoji-data's emoji.json.

    python3 tools/slack_shortcodes.py <emoji.json> > third-party/emoji-data/slack_shortcodes.tsv

One line per shortcode Slack knows, `<shortcode>\t<codepoints>`, the
codepoints as emoji.json writes them (hex, `-`-separated). A skin-tone
variation is keyed the way Slack names it, `+1::skin-tone-2` (or
`skin-tone-2-3` for an emoji with two people), which is also the name a
reaction carries.
"""

import json
import sys

TONE = {"1F3FB": "2", "1F3FC": "3", "1F3FD": "4", "1F3FE": "5", "1F3FF": "6"}


def tone_suffix(key: str) -> str:
    return "skin-tone-" + "-".join(TONE[part] for part in key.split("-"))


def rows(emoji: list[dict]) -> list[tuple[str, str]]:
    out = []
    for e in emoji:
        for name in e["short_names"]:
            out.append((name, e["unified"]))
            for key, variation in (e.get("skin_variations") or {}).items():
                out.append((f"{name}::{tone_suffix(key)}", variation["unified"]))
    return sorted(out)


def main() -> None:
    with open(sys.argv[1], encoding="utf-8") as f:
        emoji = json.load(f)
    table = rows(emoji)
    names = [name for name, _ in table]
    if len(names) != len(set(names)):
        sys.exit("emoji.json names a shortcode twice")
    for name, codepoints in table:
        sys.stdout.write(f"{name}\t{codepoints}\n")


if __name__ == "__main__":
    main()
