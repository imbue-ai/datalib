# Icons

One file per icon. The file's name without its extension is the token
the catalogs name (`icon: "gmail"` in `config/catalog.ts`,
`Some("gmail")` in `backend/columns/src/source_catalog.rs`), and
`config/icons.ts` picks every file here up by that name. The README's
source grid uses these same files by path.

**One file serves both themes.** A mark that would vanish on one
background — a solid black one on the dark theme — flips its own fill:

```svg
<style>@media (prefers-color-scheme: dark) { path { fill: #ffffff } }</style>
```

The app follows the system's light or dark setting and has no switch of
its own, and GitHub's README evaluates the same query, so the mark and
the page always agree. Don't keep a `_dark` copy anywhere.
`scripts/lint_repo.py` (check 13) measures every SVG against the light
and dark backgrounds and fails one that doesn't show up on either.

## Where each mark comes from

- **Brand marks** are [Simple Icons](https://simpleicons.org) (CC0) in
  the brand's colour, except:
  - `airvisual.svg`: IQAir publishes no vector mark. This redraws the
    favicon dashboard.iqair.com serves, a white cross on red, from its
    48px bitmap.
  - `beeper.png`: Beeper publishes no vector mark. This is beeper.com's
    own favicon.
  - `fastmail.svg`: not on Simple Icons. This is the vector favicon
    fastmail.com serves.
  - `garmin.svg`: the Simple Icons mark is Garmin's black wordmark,
    unreadable at 18px. This is the delta from it on its own, in Garmin
    blue.
  - `yolink.png`: YoLink publishes no vector mark. This is the circle
    logo shop.yosmart.com serves as its favicon.
- **Generic glyphs** (`apple_photos`, `calendar`, `contacts`, `fsindex`,
  `media`, `pdf`, `perseus`, `search`, `system`) are
  [Material Design Icons](https://pictogrammers.com/library/mdi/)
  (Apache-2.0) in a mid grey that reads on both themes. Apple publishes
  no vector app icons, and the rest are not products. `search` marks the
  unified index group, and `system` the System group.
- **Our own:**
  - `claude_code.svg` and `codex.svg`: a terminal glyph in Claude's
    colour and in OpenAI's, so a list that shows Claude Code beside Claude,
    or Codex beside ChatGPT, tells them apart.
  - `diff.svg`: a diff group's mark, the three outcomes a diff sorts rows
    into.

Nobody wrote down where `email.svg` and `sms.svg` came from.
