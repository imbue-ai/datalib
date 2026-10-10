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
export; the TNG fixture under `tests/fixtures/facebook_tng/` reproduces
every file render reads, in the same shapes and at the same paths.

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
  messages/inbox/<name>_<id>/message_1.json, photos/…
  messages/filtered_threads/…, message_requests/…, e2ee_cutover/…
  messages/stickers_used/…, messaging_settings.json, …
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
- **A Messenger conversation**, `messages/<folder>/<dir>/message_<n>.json`:
  `participants[{name}]`, `title`, `thread_path`, `is_still_participant`
  (and `is_pending` in `message_requests/`, `joinable_mode` in some
  groups), and `messages[]`, **newest first**. A message has a
  `sender_name` and a `timestamp_ms` — milliseconds, where every other
  file counts seconds — and any of `content`, `photos[{uri}]`,
  `share{link, share_text}`, `sticker{uri}`, `reactions[{actor,
  reaction, timestamp?}]` and `is_unsent`. Nothing in it has an id. The
  folder is where Facebook filed the conversation: `inbox`,
  `filtered_threads` (what it took for spam), `message_requests`,
  `e2ee_cutover` (moved to end-to-end encryption). The directory is the
  other side's name squashed to lowercase, `_`, and the conversation's
  id (`williamriker_1000000001`); a deleted account's is
  `facebookuser_<id>`, or the bare id when the export names nobody.

Every `media` object names its file by an export-relative `uri`
(`your_facebook_activity/posts/media/…`) and carries a
`creation_timestamp`, an optional `title` (an album photo's title is the
album's name) and an optional `description` (the caption).

Three things every file does that a reader has to know:

- **Non-ASCII text is mis-encoded.** Facebook writes each UTF-8 *byte* as
  its own `\u00XX` escape, so ✊ arrives as `â`. The
  ingest undoes it for every string in every record (`mojibake.rs`), so
  the raw store holds the text the person wrote. A string is only
  rewritten when all its characters fit in one byte and the bytes decode
  as UTF-8, which leaves correct text — and genuine latin-1 — alone.
- **A tagged person is markup.** `@[<id>:2048:<Name>]` in a caption or
  description; render shows the name.
- **A person is a name.** No file gives a person an id. An account
  deleted since is written `Facebook user` or as an empty name, and a
  group can hold several of them, indistinguishable.

## What ingest does

`ingest::fetch` walks every `*.json` under the export root. Each file
becomes one table, named for its path with the extension dropped,
non-alphanumerics collapsed to `_` and a trailing `_<digits>` removed —
that suffix is the chunk index Facebook splits a long file on, and every
chunk belongs in the one table
(`your_facebook_activity_posts_your_posts_check_ins_photos_and_videos`,
`your_facebook_activity_posts_album`). Each record is one row: `id` is
the record's `fbid` when it has one, else a uuidv5 over the table and
the record's canonical JSON; `payload` is the record. An `fbid` is not
always one record's alone — two saved versions of one post can share
it — so where an `fbid` names records that differ, each is keyed by
its content instead, and neither overwrites the other. `schema_raw.rs`
names the tables render reads and pins each to the path it comes from.

Messenger is the exception (`ingest/messenger.rs`): every conversation
lands in two tables, not a table each. `messenger_threads` has a row
per conversation, keyed by the digits its directory ends in, holding
`{thread_id, folder, thread}` — the file without its messages.
`messenger_messages` has a row per message, keyed
`<thread id>:<timestamp_ms>:<n>`, holding `{thread_id, message}`; `n`
counts messages of one millisecond from the oldest, so a key stays put
as newer messages arrive. A message's media edges are its own.

A store written before the Messenger tables held each conversation file
as one row of a table named for its path. Rung 1 of `schema_raw::LADDER`
splits those rows into the two tables, moves each media edge (and the
bytes it names) to the message that sent the file, and drops the old
tables.

The whole run is one snapshot in one transaction: every row is upserted,
then every row of each table the export no longer holds is deleted. A
commit landing at any point therefore sees last run's table or this
run's, never an emptied one — the rule in `docs/dev/plans/one_mode.md`.
What holds the deletions back is under "When part of a run fails"
below.

After the rows are committed, every `uri` in every record is read off
disk once and stored in the sibling `blobs.sqlite`, with one
`media_blobs` edge per `(record, uri)` — a photo an album and a post both
reference is stored once and reached twice. Bytes already in the CAS are
found through the edge table's `blake3` and not re-read; a `uri` no file
answers to (the export left it out, as it does for some videos) is a
`not_found` warning on its edge, `media_blobs:<record>#<uri>`, and a
`media_missing` count, never a failed run. A file that is there and
will not read is an error on its edge instead. Either clears the run
the file reads. The bytes are held in memory only up to 32 MB between
flushes, since a real export's media runs to gigabytes.

Every run reads the whole export, so an edge's `_bookkeeping` sidecar
is stamped the first time it lands and left alone after, and a chunk
read again with the same bytes keeps its `ingested_files` stamp. A
`uri` with no file is tried again every run, and the same warning
recorded again changes nothing either. Reading an unchanged export
again commits nothing (`reading_an_unchanged_export_again_commits_nothing`).

An edge follows its record. In the transaction that prunes the records,
a deleted record's edges go, and so do those of a record read this run,
in a table that pruned, to a `uri` it no longer names
(`media_edges_removed` in the step summary). A record in a table held
back this run (below) keeps its edges, as it keeps its row.

Files nothing renders — ad preferences, login history, search history,
notification settings — are mirrored all the same. They are the record
of what Facebook holds about the account, and a table is cheap.

## When part of a run fails

Every run reads the whole export, so each of these is a `problems` row
that the next run which reads the thing clears:

- **A file that will not read or parse** is a `listing:file <path>` row.
  Its table is upserted but not pruned this run: chunks of one table
  (`album/0.json`, `album/1.json`) share it, and the rows of the chunk
  that failed are missing from this run's set without having gone.
- **A chunk missing from a table the export has the rest of** — an
  export unpacked only in part — is a `listing:file <path>` row, and the
  table is upserted but not pruned. Each run records in `ingested_files`
  (scope `facebook/<table>`) the chunk files it read a table from, and a
  table prunes only while every one of them is there. A table none of
  whose files is in the export was left out of it, and keeps its rows.
  An export that really has fewer parts of a table than the last one
  looks the same as a partial unpack; resetting the source clears the
  record.
- **A directory the walk could not list** (or an entry it could not
  stat) is a `listing:files` row, and no table is pruned that run.
- **An export path with nothing at it** fails the run: there is nothing
  to mirror, and an empty walk would read as an export that holds
  nothing.

## What render does

One open of the store per pass, loading the nine tables and diffing
them against the render cursor, then six feeds:

| feed | table | one document per |
|---|---|---|
| posts | `…posts_your_posts_check_ins_photos_and_videos` + `…posts_on_other_pages_and_profiles` | post: the text, its media as attachments, a `📍 Place — address` line per check-in (the export lists a place twice, with and without its page URL; the one with the URL wins), a life event as a bold title and description, and `— with A, B` for tags |
| albums | `…posts_album` | album: the description first, then every photo in creation order, captioned where the photo has one of its own |
| comments | `…comments_and_reactions_comments` | year: the comment, with Facebook's sentence about it in italics beneath, any photo attached and any link |
| reactions | `…comments_and_reactions_likes_and_reactions` | year: `👍 X liked Y's post.`, the URL as the header's `↗` |
| friends | `connections_friends_your_friends` | friend, as a contact in one "Friends" group with a "Friends since" field, and their name as their handle |
| Messenger | `messenger_threads` + `messenger_messages` | year of a conversation: each message with its photos, sticker, shared link (`🔗`) and reactions; an unsent one as a note. The project says which folder (`Messenger`, `Messenger · requests`, …) |

Comments and reactions are bucketed by year because the export does not
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

A person on Messenger is their name, written as a `facebook:name/`
handle; a friend carries the same one, so the contacts app finds them
as one person. An account deleted since (`Facebook user`, or an empty
name) is `facebook:deleted/<conversation id>` where the conversation
lists it as its one deleted account, the case of a one-to-one
conversation. Where a group lists several, each message and reaction
from one says `Facebook user (one of N deleted accounts here)` and has
no handle: nothing in the export tells them apart.
`docs/dev/contacts.md` has the handle's rules.

### Edits

`posts/edits_you_made_to_posts.json` and
`comments_and_reactions/your_comment_edits.json` hold the versions a
post or a comment was saved in — on a real export, the first at the
moment it was posted and the last the text it has now — in the
`label_values` shape, with the text under `Text` (a comment's also has
`Caption` and `Content state`, empty on the account we have). Neither
names the post or comment it is a version of. `edits.rs` ties each
version to the most alike post or comment made no later than it
(`similar`'s ratio at least 0.6), keeping versions that share an `fbid`
together. On the page, the earlier versions fold into one
"✎ Another version" just above the text they became; the version that
is the text now is not shown twice. An edit no post or comment matches
is of one the export no longer has: a post of its own, or a comment at
its own time, with its last version shown and a note saying so. Every
post (and the comments feed) reads every edit row, since a new edit
may be any one's.

A post's `update_timestamp` is not an edit: on a real export it is the
post's own time on all but one post in sixty-three. A post on another
page carries a `Last modified`, shown as `Edited <day>` when it is not
the day it was posted.

### What render reports

Facebook adds fields without notice, and a field render does not read
would otherwise vanish without a word. So render checks every Messenger
message and conversation, every post (both shapes) and every comment
against the keys it reads, and each key it does not read is a warning
row in `problems`, on the document and at the item's own section
(`item_uuid`), with the field, its JSON pointer and — never the value —
its shape: `int`, `string(12 chars)`, `object{a,b}`. A `label_values`
entry whose label render does not use is `label_values:<label>`. Beside
those:

| row | severity | means |
|---|---|---|
| `timestamp_ms`, `CoercionFailed` | warning | a message with no usable time |
| `uri` / `reaction`, `UncoveredType` | warning | a media entry with no file, a reaction with no emoji |
| `is_geoblocked_for_viewer`, `is_unsent_image_by_messenger_kid_parent` | warning | the export withholds the message |
| `message`, `Noted` | info | a message with nothing to show; the sample lists its keys |
| `participants`, `Noted` | info | several deleted accounts in one conversation |
| `sender_name`, `Noted` | info | a sender the participants do not list, once per sender |
| `Text`, `Noted` | info | an edit of a post or comment the export no longer has |

Counting them over a real export says what to build next:

```sh
datalib-doltlite -readonly <root>/unified_index/grid_index/db.doltlite_db \
  "SELECT severity, field, sample, count(*) FROM problems
   WHERE source_id = '<the source id>' GROUP BY 1,2,3 ORDER BY 4 DESC"
```

`RENDER_VERSION` is in `facebook_render/src/common.rs`.

## Not built

- Photos and videos outside posts and albums
  (`your_uncategorized_photos.json`, `your_videos.json`), places, search
  history: in the raw store, not rendered.
- A `uri` is stored when a record points at it. Media files the export
  ships but no JSON record names are not mirrored.
