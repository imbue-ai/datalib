# emoji-data, Slack's shortcodes

Slack names its standard emoji after
[iamcal/emoji-data](https://github.com/iamcal/emoji-data) (MIT, see
`LICENSE`): `:thinking_face:`, `:flag-ca:`, `:+1::skin-tone-2:`. Those
names often differ from GitHub's. `slack_shortcodes.tsv` is that
project's `emoji.json` at tag `v16.0.0` (commit
`2771d0b1b3af25c069086e68e38f901c3dda8bdf`), cut down to shortcode →
codepoints by `tools/slack_shortcodes.py`. The Slack render embeds it
(`datalib/backend/etl/providers/slack_render/src/render/emoji.rs`).

To move to a newer release, download its `emoji.json`, run

```sh
python3 tools/slack_shortcodes.py emoji.json > third-party/emoji-data/slack_shortcodes.tsv
```

and put its tag and commit above. Bump the Slack render's `RENDER_VERSION`
so every Slack document re-renders.
