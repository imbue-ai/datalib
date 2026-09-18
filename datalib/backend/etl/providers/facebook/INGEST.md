# `facebook` — a "Download your information" export

The `facebook` source mirrors the export Facebook produces under
**Accounts Center → Your information and permissions → Download your
information**, in its **JSON** format. It is file-backed: the one
knob is `export.path`, the directory the zips were unpacked into. The
HTML format of the same export is not read — the two carry the same
records, and JSON is the one a program can trust.

The ingest crate is `datalib_etl_facebook` (this directory); the render
crate is `datalib_etl_facebook_render`; the config schema is
`datalib_etl_facebook_config`. The shapes below were read off a real
export requested on 2026-06-19, and the TNG fixture under
`tests/fixtures/facebook_tng/` reproduces them file for file.

## What the export looks like

Eight category directories, each holding JSON files two or three levels
down, plus a `no-data.txt` wherever a category was empty and a
`start_here.html` at the root:

```
ads_information/
apps_and_websites_off_of_facebook/
connections/friends/your_friends.json
logged_information/
personal_information/profile_information/profile_information.json
preferences/
security_and_login_information/
your_facebook_activity/
  comments_and_reactions/comments.json
  comments_and_reactions/likes_and_reactions.json
  comments_and_reactions/likes_and_reactions_1.json
  posts/album/0.json, 1.json, …
  posts/media/<Album>_<id>/<photo id>.jpg, posts/media/your_posts/…
  posts/posts_on_other_pages_and_profiles.json
  posts/your_posts__check_ins__photos_and_videos_1.json
  posts/your_uncategorized_photos.json
  posts/your_videos.json
  messages/…              (settings only, on the account we have)
```

Three record shapes recur across the files:

- **An array of records** (`your_posts__…_1.json`,
  `likes_and_reactions_1.json`), or an object wrapping one under a
  versioned key (`{"comments_v2": […]}`, `{"friends_v2": […]}`,
  `{"searches_v2": […]}`). Each record has a `timestamp` in **seconds**,
  a `title` that is Facebook's own sentence about it ("X added 3 new
  photos."), a `data[]` list of one-key objects (`{"post": …}`,
  `{"comment": {…}}`, `{"reaction": {…}}`, `{"backdated_timestamp": …}`)
  and an `attachments[]` list whose `data[]` entries are `{"media":
  {…}}`, `{"place": {…}}`, `{"life_event": {…}}`, `{"external_context":
  {…}}` or `{"text": …}`.
- **`label_values`** (`likes_and_reactions.json`, `posts_on_other_pages
  _and_profiles.json`, `places_you_have_been_tagged_in.json`): a list of
  `{"label": …, "value": …}` pairs, some with a `timestamp_value` or a
  `media` list instead of a `value`, some nesting a `dict` list under a
  `title`. These records carry Facebook's own id in `fbid`.
- **One object** (`album/0.json`, `profile_information.json`).

Every `media` object names its file by an export-relative `uri`
(`your_facebook_activity/posts/media/…`) and carries a
`creation_timestamp`, an optional `title` (an album photo's title is the
album's name) and an optional `description` (the caption).

Two things every file does that a reader has to know:

- **Non-ASCII text is mis-encoded.** Facebook writes each UTF-8 *byte* as
  its own `\u00XX` escape, so ✊ arrives as `â`. The
  ingest undoes it for every string in every record (`mojibake.rs`), so
  the raw store holds the text the person wrote. A string is only
  rewritten when all its characters fit in one byte and the bytes decode
  as UTF-8, which leaves correct text — and genuine latin-1 — alone.
- **A tagged person is markup.** `@[<id>:2048:<Name>]` in a caption or
  description; render shows the name.

## What ingest does

`ingest::fetch` walks every `*.json` under the export root. Each file
becomes one table, named for its path with the extension dropped,
non-alphanumerics collapsed to `_` and a trailing `_<digits>` removed —
that suffix is the chunk index Facebook splits a long file on, and every
chunk belongs in the one table
(`your_facebook_activity_posts_your_posts_check_ins_photos_and_videos`,
`your_facebook_activity_posts_album`). Each record is one row: `id` is
the record's `fbid` when it has one, else a uuidv5 over the table and
the record's canonical JSON; `payload` is the record. `schema_raw.rs`
names the seven tables render reads and pins each to the path it comes
from.

The whole run is one snapshot in one transaction: every row is upserted,
then every row of each table the export no longer holds is deleted. A
commit landing at any point therefore sees last run's table or this
run's, never an emptied one — the rule in `docs/dev/plans/one_mode.md`.
There is no cursor, so `reset_and_redownload` has nothing to clear.

After the rows are committed, every `uri` in every record is read off
disk once and stored in the sibling `blobs.doltlite_db`, with one
`media_blobs` edge per `(record, uri)` — a photo an album and a post both
reference is stored once and reached twice. Bytes already in the CAS are
found through the edge table's `blake3` and not re-read; a `uri` no file
answers to (the export left it out, as it does for some videos) is a
warning and a `media_missing` count, never a failed run. The bytes are
held in memory only up to 32 MB between flushes, since a real export's
media runs to gigabytes.

Files nothing renders — ad preferences, login history, search history,
notification settings — are mirrored all the same. They are the record
of what Facebook holds about the account, and a table is cheap.

## What render does

One open of the store per pass, loading the seven tables and diffing
them against the render cursor, then five feeds:

| feed | table | one document per |
|---|---|---|
| posts | `…posts_your_posts_check_ins_photos_and_videos` + `…posts_on_other_pages_and_profiles` | post: the text, its media as attachments, a `📍 Place — address` line per check-in (the export lists a place twice, with and without its page URL; the one with the URL wins), a life event as a bold title and description, and `— with A, B` for tags |
| albums | `…posts_album` | album: the description first, then every photo in creation order, captioned where the photo has one of its own |
| comments | `…comments_and_reactions_comments` | month: the comment, with Facebook's sentence about it in italics beneath, and any photo attached |
| reactions | `…comments_and_reactions_likes_and_reactions` | month: `👍 X liked Y's post.`, the URL as the header's `↗` |
| friends | `connections_friends_your_friends` | friend, as a contact in one "Friends" group with a "Friends since" field |

Comments and reactions are bucketed by month because the export does not
say which post they were left on in any form we can resolve: a comment
record has a `title` sentence and no link, a reaction has a URL to a post
that is usually somebody else's. LinkedIn's export names the post, so it
gets a thread per post; this one cannot.

Reactions come in two shapes at once: `likes_and_reactions.json` is
`label_values` rows (reaction, URL, the target's name, an `fbid`) and
`likes_and_reactions_1.json` is `data[].reaction` rows (reaction, actor,
a `title` sentence). On the account we have both files describe the same
events. Render merges them on `(timestamp, reaction)` and takes what each
has — the sentence from one, the URL from the other — so a reaction is
one row, not two.

The `account` on every row is the profile's first listed email, else its
full name; every item is written under the full name. Every document
declares the profile row beside its own, so a name change re-renders
everything, which is what it should do.

`RENDER_VERSION` is in `facebook_render/src/common.rs`.

## Not built

- **Messenger.** `your_facebook_activity/messages/inbox/<thread>/
  message_1.json` is the most valuable part of a real account's export
  and the test account has none: its `messages/` holds settings files
  only. The files are mirrored to the raw store like any other JSON, but
  nothing renders them, and no shape is pinned here because none has
  been seen. Building it wants an export from an account that has
  actually sent a message.
- Photos and videos outside posts and albums
  (`your_uncategorized_photos.json`, `your_videos.json`), places, search
  history: in the raw store, not rendered.
- A `uri` is stored when a record points at it. Media files the export
  ships but no JSON record names are not mirrored.
