# docs/assets

Images the docs use and the app does not. The README's source grid
takes its icons from `datalib/ui/src/assets/`, the same files the app
shows, and each of those serves both themes itself
(`datalib/ui/src/assets/README.md`). What lives here is only what the
app has no icon for:

- `google_chat.svg` (Simple Icons, CC0) and `google_voice.png` (the
  product mark gstatic.com serves for voice.google.com): two products
  of the one `google_takeout` source, which the app shows as one entry.

Never a copy of an app icon: `scripts/lint_repo.py` (check 13) refuses a
file here that shares a name with one there.
