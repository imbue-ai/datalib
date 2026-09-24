# docs/assets

Images the docs use and the app does not. The README's source grid
takes its brand marks from `datalib/ui/src/assets/` (one source of
truth, shared with the Manage screen); what lives here is only what
the app has no cell for:

- `*_dark.svg` — white-on-dark twins of the two solid-black marks
  (GitHub, Notion), for GitHub's dark theme. A twin is a copy: change
  the mark in `datalib/ui/src/assets/` and its twin here together.
- `google_chat.svg` (Simple Icons, CC0) and `google_voice.png` (the
  product mark gstatic.com serves for voice.google.com) — two products
  of the one `google_takeout` source, which the app shows as one entry.
