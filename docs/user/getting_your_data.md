# Getting your data

One section per source, in alphabetical order, saying what the source
mirrors and how to get a copy of your data where the pipeline can read
it: either credentials stored in `latchkey`, or an export on disk. For
the config file and running the sync, see the
[first-time user guide](first_time_user.md); for one fully-commented
config entry per source, [`all_sources.toml`](config_examples/all_sources.toml).

Each section opens with the source's `type`, the word that goes in the
config and picks the code that runs, and where the data comes from:
one table on the ingest step, named under the type. `api` is the
product's own API, `export` an unpacked export, `backup` a phone backup,
`fswalk` a folder to scan; a type that is not one product names its
ways in (email's are `jmap`, `gmail`, `mbox` and `apple_mail`). A table
that reads files carries its own `path`.

Conventions: exports land under `~/backups/`; adjust to taste and point
the source at them. `latchkey` is the one the Datalib installer put on
your `PATH`. Wherever a command takes a secret, it is written as
`$(pbpaste)`: copy the secret to your clipboard, then run the command,
and your shell history keeps `$(pbpaste)` rather than the secret.

The credentials `latchkey` stores are the same session your browser
has. Anything that can run commands as you can use them to act as you
on that service; read
[who can use your sign-ins](first_time_user.md#who-can-use-your-sign-ins-and-why)
before storing any.

The `latchkey auth` commands here sign in on the machine you run them
on. If `LATCHKEY_GATEWAY` is set in your environment, a latchkey gateway
holds the credentials instead, and those commands may be refused or sign
in somewhere else: sign in, or ask for access, wherever that gateway is
managed. Either way, `datalib-step probe <type>` says whether the
sign-in works; for a type reached more than one way, name the way with
`--method` (`datalib-step probe calendar --method google`).

## Apple Mail (Local)

`type = "email"` with an `apple_mail` table — every account of the Mail
app on a Mac, read from Mail's own folder, `~/Library/Mail`. Copies
every message Mail has downloaded, with its mailboxes (a Gmail account's
labels among them), whether it was read, flagged or answered, and its
attachments. No sign-in, and nothing is fetched from the mail server: a
message or attachment Mail has not downloaded yet is copied by the sync
after Mail has it, and until then the source shows a warning counting
them.

macOS protects Mail's folder. In the app, choose it with the picker in
the "Apple Mail" form — that is what grants Datalib access (Cmd-Shift-G
in the dialog reaches `~/Library/Mail`). A terminal running
`datalib-pipeline` needs Full Disk Access instead (System Settings → Privacy
& Security).

Each account is copied under its own id, made from the name of its folder
under `~/Library/Mail/V10`; a message sent to two of your accounts is
copied once for each. To copy just one account, set `apple_mail.account`
to its folder's name. Mail keeps no Gmail ids, so a Gmail account copied
through Mail and through [Gmail](#gmail) is two separate copies.

### Limitations

- **The copy holds only what the Mail app has already downloaded.** A
  message Mail lists but has not downloaded yet is left out, and is
  counted in a warning on the source, until a later sync finds the file.
  Cause: the format. This method reads Mail's own folder and never asks
  the mail server for anything. What changes it: let Mail finish
  downloading, then sync again.
- **Some attachments are missing, most of them calendar invitations.**
  Mail keeps some attachments in separate files and never downloads
  others. A part with no file is left empty in the stored message, and
  the email gets a warning naming the parts. On one measured Gmail
  account, nine in ten of the missing parts were calendar invitations.
  Cause: the format (what Mail chose to keep). Nothing in Datalib
  changes it.
- **By default every account in Mail is copied, and none of them knows
  your address.** Each account is filed under an id made from Mail's
  folder name (`apple_mail_1f0b8a52`), because Mail's folder does not
  record the account's address. So `from:me` and `to:me` do not find
  your mail from this source alone. Cause: the format. What changes it:
  set `apple_mail.account` to one account's folder name and
  `apple_mail.email_address` to its address (one source per account), or
  mark your address as you on a contact.
- **There is no `since`: everything Mail holds is copied.** Cause:
  Datalib's implementation (this method has no window setting).
  `only_extract_labels` (unset: every mailbox) narrows it by mailbox.
- **`blob_size_limit_bytes` is ignored.** Every message is stored whole,
  however large. Cause: Datalib's implementation (only the Gmail and
  JMAP methods read that setting). Nothing changes it.
- **A Gmail or Fastmail account read through Mail is a separate copy
  from the same account read through its own API.** Mail keeps no Gmail
  message or thread ids, so the two never merge into one set of rows,
  and a message sent to two of your accounts is copied once for each.
  Cause: the format. Nothing changes it; give each its own source.
- **A conversation is Mail's grouping, not Gmail's or the server's.**
  Messages are grouped into threads by Mail's own conversation number. A
  message Mail put in no conversation is a thread on its own. Cause: the
  format.
- **Deletions are seen on the next sync.** A message Mail no longer
  lists, or marks deleted, leaves the copy, unless some message file
  failed to read in that sync. `when_deleted_upstream` is not offered.
  Cause: Datalib's implementation (only `claude` and `chatgpt` offer
  it).
- **Read, flagged and answered marks, and moves between mailboxes, are
  seen on the next sync.** Mail's index is read whole every sync. Cause:
  the format. Nothing else is needed.

### Reading the copy

One rendered document is one email conversation (thread) in one
account. Its grid rows are `kind = "Email Thread"` for the document
and `kind = "Email"` for each message. Each account and each label is
also a place in the containers tree (`Email Account`, `Email Label`).
The render step's `only_render_labels` (unset: every thread) limits
which threads render; it does not limit the download. No time series or
occurrences.

The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`;
each message's full `.eml` is in `blobs.sqlite` beside it. Messages are
in `emails`, conversations in `threads`, labels in `mailboxes`, and
which message is in which label in `email_mailboxes`; read and muted
are in `read_marks`, not in the labels. A thread is the rows of
`emails` with one `thread_id`; order them by `datetime(received_at)`,
never by the text, which keeps each sender's UTC offset. "Me" is the
account's address (`accounts.email_address`), which is unknown for an
mbox or Apple Mail account unless the config sets it; compare it with
the addresses in `from_json`. For Fastmail, `to_json`, `cc_json` and
`in_reply_to` are NULL, so read recipients from the `.eml`.

## Apple Messages (Local)

`type = "apple_messages"` — reads the Messages app's own `chat.db` on a
Mac, or a copy of it (`messages.path`). Mirrors iMessage and SMS chats
with tapbacks; attachments are listed by name, their bytes are not
copied.

Point it at the folder, `~/Library/Messages`, or at a `chat.db` file
directly. An iPhone backup's `3d0d7e5fb2ce288813306e4d4636395e047a3d28`
is the same database and works too. macOS protects `~/Library/Messages`:
in the app, choose the folder with the picker — that is what grants
Datalib access (Cmd-Shift-G in the dialog reaches it). Choosing
`chat.db` alone is not enough, because the database's `chat.db-wal`
sits beside it and has to be read too. From a terminal, the terminal needs
Full Disk Access (System Settings → Privacy & Security); an ingest that
reports "Operation not permitted" is missing that, not the file.

The whole database is mirrored table for table with its history, and
rows are only ever added, so a message deleted on the Mac stays in the
copy.
Every chat renders as one page, however long it runs.

### Limitations

- **A message deleted on the Mac, or let go by "Keep messages", stays in
  the copy and keeps showing on its chat's page.** Cause: Datalib's own
  implementation, on purpose. The store is upserted (each table's rows
  are written by their key and none is ever removed), because Messages
  deletes old messages when "Keep messages" is set to 30 days or a year
  and keeping them is the reason to mirror the database. A deletion is
  never noticed and `when_deleted_upstream` is not offered. Nothing
  changes it.
- **Nothing older than what `chat.db` holds on this Mac is in the copy.**
  Cause: the file format. The source reads the Messages database on
  disk and nothing else, so a message the Mac never had, or deleted
  before the first sync, is not there. There is no `since` option: every
  row `chat.db` holds is mirrored. Nothing changes it.
- **Attachments are listed, not copied.** Each one shows its name, type,
  size and the path it had under `~/Library/Messages/Attachments/`, with
  a "not yet fetched" placeholder; no bytes are kept. Cause: Datalib's
  implementation (that folder is often tens of gigabytes, and nothing
  copies it yet). Nothing changes it.
- **People appear as phone numbers and email addresses, not names.**
  Cause: the file format. `chat.db` holds only each handle's number or
  address; contact names live in the Contacts app. A chat with no name
  of its own is titled by its members' numbers. Linking a handle to a
  person in the contacts app is the only way to get a name.
- **An inline reply is linked to the first message of its thread, not
  to the message it answered.** Cause: the file format. `chat.db`
  records only the guid of the thread's first message
  (`thread_originator_guid`) on a reply. A database old enough to have
  no such column shows no reply links at all. Nothing changes it.
- **A message that no chat holds is left out of every page.** Cause:
  Datalib's implementation (a page is a chat). It is reported as a
  problem on `message:<rowid>`, saying whether no join row names it or
  the join names a chat the `chat` table lacks. It reappears if a later
  sync finds it in a chat.
- **Some message bodies come out empty.** Since macOS Ventura most
  bodies are in `attributedBody`, an archived NSAttributedString, rather
  than `text`. Cause: Datalib's implementation, which decodes the two
  layouts it knows. A body it cannot decode renders as a message with no
  text and is reported as a problem on that message, with the body's
  size and none of its bytes. Formatting, mentions and other attribute
  runs inside the archive are never read.
- **A tapback whose target message is not in the same chat's copy is
  dropped without a word.** Cause: Datalib's implementation. A
  withdrawn tapback is removed from the page rather than shown struck
  out. Nothing changes it.
- **A message you sent does not record who it was sent to.** Messages
  from others are marked as sent to you (`to:me`); your own carry no
  recipients. Cause: Datalib's implementation. Nothing changes it.
- **The copy is only as fresh as the last sync, and the app's own
  bookkeeping is left out by default.** `skip_churn` (on by default)
  leaves out the app's counters and settings, the Spotlight and CloudKit
  work queues, the cloud-sync tombstone tables (`sync_*`, `unsynced_*`)
  and every `index_state` column. Cause: Datalib's implementation; with
  it off, every sync makes a new commit even when nobody messaged.
  `recoverable_message_part` (the "recently deleted" text) is kept.
- **If the snapshot of the live database fails, the copy may be torn,
  and only the log says so.** Datalib reads `chat.db` through a
  `VACUUM INTO` copy; when that fails it falls back to copying the file
  and its `-wal`/`-shm` files, which can be caught mid-write while
  Messages runs. Cause: Datalib's implementation. The warning goes to
  the log, not to `problems`. Quitting Messages before a sync avoids it.

### Reading the copy

Each chat is one rendered document, however long it runs. Its grid rows
are one `Messages Chat` row for the chat, one `Messages Message` row per
message (a group event, such as someone added or the group renamed, is a
message row too, drawn as a note), and one `Messages Tapback` row per
tapback. Nothing is rendered as time series or occurrences.

The raw store, `<data_root>/<source id>/ingest/entities.doltlite_db`, is
`chat.db` itself, table for table: `message`, `chat`, `handle`,
`attachment` and the join tables `chat_message_join`, `chat_handle_join`
and `message_attachment_join`, under Apple's own names.

- **Me**: `message.is_from_me = 1`. A message from someone else names
  them by `message.handle_id` → `handle.ROWID`, whose `id` is the phone
  number or address.
- **A chat's messages** are the `message` rows that `chat_message_join`
  puts in it (`chat_id` → `chat.ROWID`, `message_id` → `message.ROWID`).
  The upstream identity is the guid: `chat.guid` for a chat,
  `message.guid` for a message.
- **The text** is usually NULL in `message.text`; the body is the
  archived `attributedBody` blob, which SQL cannot decode. Read bodies
  from the rendered document, not the raw store.
- **Dates** in `message.date` are nanoseconds since 2001-01-01 UTC
  (seconds in a database from before about 2011); `0` means no date.

## Apple Photos (Local)

`type = "apple_photos"` — reads an Apple Photos `.photoslibrary`
bundle (`library.path`). Mirrors the library's database — every asset,
album, person, face, keyword and edit — as a versioned backup;
download-only, nothing is rendered.

On macOS the library is `~/Pictures/Photos Library.photoslibrary` and
it is a protected location: in the app, choose it with the picker
rather than typing the path, and if a sync still reports "Operation not
permitted", grant Datalib Full Disk Access in System Settings → Privacy
& Security.

What gets mirrored is `database/Photos.sqlite`, about ninety tables of
Photos' own schema, table for table, with only changed rows stored on
each run. The originals sit beside it at
`<library>/originals/<X>/<UUID>.<ext>`; a [local media](#media-local) source pointed
at that folder gives you their EXIF and content hashes.

### Limitations

- **The photos and videos themselves are not copied, only the library's
  database.** Cause: Datalib's implementation. The source reads
  `database/Photos.sqlite` inside the `.photoslibrary` bundle and no
  other file in it, so the originals under `originals/` and the edited
  versions under `resources/renders/` are not in the copy. A
  [local media](#media-local) source pointed at `<library>/originals`
  reads the files' metadata and hashes.
- **Nothing is rendered: no photo appears in the grid or in search.**
  Cause: Datalib's implementation; Apple Photos has no render. The grid
  shows only Datalib's storage report for the source, and its item
  count is the number of `ZASSET` rows. Every question about the library
  has to be answered from the raw store with SQL.
- **A photo deleted from the library leaves the current copy at the next
  sync, and only the store's history keeps it.** Cause: Datalib's
  implementation. Each sync empties the mirror and fills it again from
  `Photos.sqlite`, so the current tables are the library as it is now; a
  row that is gone is still in `dolt_history_<table>` and every earlier
  commit. `when_deleted_upstream` is not offered. Nothing changes it.
- **By default, Core Data's change log, the Photos daemons' work queue
  and counters, and every row's `Z_OPT` column are left out.**
  `skip_history`, on unless set, leaves out the tables `ACHANGE`,
  `ATRANSACTION`, `ATRANSACTIONSTRING`, `ZBACKGROUNDJOBWORKITEM`,
  `Z_PRIMARYKEY`, `Z_METADATA`, `Z_MODELCACHE`. Cause: Datalib's
  implementation; the daemons write these even when nobody uses Photos,
  so with them in, every sync makes a commit and the store grows each
  time. The store's own history records every change instead.
- **The Memories and highlight columns of `ZASSET` change often without
  anyone touching a photo, so diffs of `ZASSET` are noisy.** Cause: the
  database format: Photos reshuffles them as it builds Memories. They
  are kept; `exclude_columns = ["ZASSET.*HIGHLIGHTBEING*"]` drops them.
- **The table and join-table names depend on the macOS version.**
  Cause: the file format. Core Data numbers its entities, so the album
  join table is `Z_33ASSETS` on one version and another number on the
  next, and the table count differs. A query written against one Mac's
  schema may fail on another's. Nothing changes it.
- **A photo's identity is `ZUUID` only where every row of the table has
  one.** Datalib keys a table on `ZUUID` when every row holds a distinct,
  non-empty value, and otherwise on Photos' `Z_PK`, which Photos
  renumbers on a library repair or an iCloud re-sync; a renumbering then
  shows in that table's history as rows removed and added. The join
  tables have no `ZUUID` and always key on `Z_PK` pairs. Cause: the file
  format (Photos never declares `ZUUID` unique). `stable_key_columns`
  (`["ZUUID"]` unless set) and `primary_keys` change it.
- **The R-tree location index is copied as rows; its storage tables are
  not, and a virtual table Datalib cannot read is skipped with only a log
  line.** Cause: Datalib's implementation. Such a skip is counted in the
  run's commit message (`virtual_tables_skipped`) but recorded in no
  problem row.
- **If the snapshot of the live database fails, the copy may be torn,
  and only the log says so.** Photos' daemons keep `Photos.sqlite` open
  in WAL mode at all times, so Datalib reads a `VACUUM INTO` copy; when
  that fails it falls back to copying the file with its `-wal`/`-shm`
  files, which can be caught mid-write. Cause: Datalib's
  implementation. `snapshot = false` reads a file that is always being
  written to; leave it on.
- **Dates are Core Data's: seconds since 2001-01-01 UTC, with the zone
  the photo was taken in kept apart.** Cause: the file format. The zone
  is in `ZADDITIONALASSETATTRIBUTES.ZTIMEZONENAME` and `ZTIMEZONEOFFSET`;
  nothing combines them for you.
- **An uncollected store grows on every sync.** `gc` (off unless set)
  runs `dolt_gc()` at the start of each sync. Cause: the storage engine
  (doltlite keeps garbage until collected).

### Reading the copy

Apple Photos renders nothing: no documents, no grid rows of its own, no
time series. The grid's only row for it is Datalib's storage report.

The raw store, `<data_root>/<source id>/ingest/entities.doltlite_db`, is
`Photos.sqlite` table for table, under Core Data's own names: `ZASSET`
(one row per photo or video), its one-to-one side tables
(`ZADDITIONALASSETATTRIBUTES` for the original file name, title and time
zone, `ZEXTENDEDATTRIBUTES` for EXIF), `ZGENERICALBUM` for albums with a
`Z_<n>ASSETS` join table, and `ZPERSON`, `ZDETECTEDFACE`, `ZFACECROP`
for people and faces.

- **A photo's identity** is `ZASSET.ZUUID`, which is also the name of
  its file: `originals/<ZDIRECTORY>/<ZFILENAME>` under the bundle.
  Other tables point at an asset by its `Z_PK`, which Photos can
  renumber; join through `ZASSET.Z_PK` at the same commit.
- **History** is the store's: `dolt_log()` lists the syncs that changed
  something, `dolt_diff_ZASSET` and `dolt_history_ZASSET` show what each
  changed.
- There is no "me": the library is one person's.

## Baby Connect

`type = "baby_connect"` — the [Baby Connect](https://www.baby-connect.com)
web app through latchkey (`api`). Copies every entry logged for each of
your children: feeds, sleeps, diapers, medicine, growth, milestones,
diary entries, and the notes typed with any of them. Each child becomes
one document, its days in order.

Baby Connect has no API, so the download makes the two requests its own
web page makes, signed in with the page's session cookies. Register the
service once, then sign in in the browser that opens; latchkey keeps the
two cookies the session is made of:

```sh
latchkey services register babyconnect \
  --base-api-url="https://app.babyconnect.com/" \
  --login-url="https://app.babyconnect.com/login" \
  --login-flow=cookie-capture \
  --login-flow-params='{"cookieKeys": ["seacloud1", "srf"]}'
latchkey auth browser babyconnect
```

Tick **Remember Me** on the sign-in page: Baby Connect then keeps the
session for a year, where without it the session can end within a day.
Once the session ends, signing in again needs you at the browser.

Baby Connect answers one day at a time, so the first sync asks once for
every day from your eldest child's birthday to today: about 365 requests
a year of use. Set `since` and `until` on the `api` table (a day, an RFC
3339 instant, or an offset such as `"-90d"`) to read less. Later syncs
read only the days not yet read, and the last two again, since a night's
feeds are often logged the next morning: a daily sync is three requests.

Baby Connect keeps each entry's time as a clock reading with no zone.
Name the zone you logged in on the render step, and daylight saving is
applied for each date:

```toml fragment
[steps.params]
time_zone = "America/Los_Angeles"
```

Without it the times are read as UTC, and the source's Manage row says
so. Changing it re-renders; it never downloads again.

Not copied: the photos attached to an entry (the store lists them, the
pictures are not downloaded). An entry deleted in the app after its day
was read stays in the copy.

### Limitations

- **The copy holds every child on the account and every kind of entry, from the eldest child's birthday to today.** There is no way to copy one child only. The days read run from `api.since` (unset: the eldest child's `BDay` on the home page) to `api.until` (unset: today). This is Datalib's own implementation. `since` and `until` narrow it.
- **An entry edited more than a day after the day it was logged for is never seen.** A sync reads again only today and yesterday; every older day is recorded as read for good the first time it is read, and is never asked for again. Cause: Datalib's implementation (Baby Connect answers one day at a time and offers no list of changes, so Datalib chose to settle a day after two days). Nothing in the config changes it; only a reset of the ingest step (`datalib-pipeline reset <data_root> <step>`) reads every day again.
- **An entry deleted in the app stays in the copy, whenever it was deleted.** The download only adds and updates entries; it never removes one, not even from today or yesterday when it reads them again, and it does not read the day's `deleted` list. Cause: Datalib's implementation. `when_deleted_upstream` is not offered. Nothing changes it.
- **When a second child is added to an account that had one, that child's entries on days already read are never read.** With one child on the account, each day is asked for that child alone; with two or more, it is asked for every child at once. The record of which days are read does not say for which children, so a day read while the account had one child is not asked for again. Cause: Datalib's implementation. Only a reset changes it.
- **If no child on the account has a birthday and `since` is not set, nothing is copied.** The run records a problem saying to set `api.since`. Cause: Datalib's implementation needs a first day.
- **Photos are not downloaded.** An entry's list of photos is kept as Baby Connect sent it, but the pictures are not fetched. Cause: Datalib's implementation. Nothing changes it.
- **Comments on an entry are stored but not shown.** They are in the raw store's payload and in no rendered document. Cause: Datalib's implementation.
- **Times have no time zone.** Baby Connect keeps a clock reading (a day and an `HHMM` time) with no zone. The render step's `time_zone` (an IANA name such as `America/Los_Angeles`, unset unless you set it) turns it into a moment. Unset, every time is read as UTC and written with `Z`, each child's document says so, and each carries a warning. A clock reading in the hour the clocks skip is moved an hour later; one in the hour they repeat is taken as the first of the two. Cause: the service's data. Set `time_zone`; changing it re-renders without downloading.
- **The first sync makes one request per day, one at a time.** Four years of use is about 1,500 requests. A later daily sync is three requests: the home page, today and yesterday. A day whose request fails is skipped, recorded as a problem, and asked for again by the next sync. Cause: Baby Connect answers one day per request.
- **The session can end, and signing in again needs a person at a browser.** Without **Remember Me** ticked at sign-in, Baby Connect's session has been seen to end within a day; with it, it lasts a year (measured). A sync with an ended session fails and copies nothing. Cause: the service.
- **A kind of entry this build does not know is shown as `Entry (<code>)`.** The names come from the codes Baby Connect's own web page uses; a code it adds later has no name here. Cause: Datalib's implementation.

### Reading the copy

- One rendered document per child, titled with the child's name, holding every entry for that child: a heading per day, then each entry in clock order with its time, its kind, who logged it, Baby Connect's sentence for it (`Txt`) and the note typed with it (`n`). A child with no entry has no document.
- Grid rows: the child's document has kind `Child`; each entry has the kind's name (`Nursing`, `Diaper`, `Sleep`, `Diary`, `Bottle`, …, or `Entry (<code>)`). The row's `channel` is the child's name and `author` is who logged it, with an `email:` handle when Baby Connect gave that person's address.
- No time series and no occurrences are rendered: a weight or a temperature is an entry with text, not a number over time.
- The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`. `entries` holds the entries, `kids` the children, `users` the people who logged them.
- An entry's child is `payload.Kid`, and who logged it is `payload.By`; both are Baby Connect ids that join `kids.id` and `users.id` once cast to text. The ids are numbers larger than 2^53, so compare them as text, never as floating-point numbers.
- An entry's day is `payload.Pdt`, a number `YYMMDD` with the year counted from 2000, and its time `payload.Utm`, a number `HHMM`. Neither has a zone.
- Nothing in the store says which `users` row is the person who signed in.

## Beeper (Local)

`type = "beeper"` — mirrors the Beeper Texts desktop app's local data
directory (`texts.path`): every chat the app holds, from Signal, Google
Chat, Slack and the rest Beeper bridges. The render step's `networks`
picks which networks to show; unset, it shows them all.

On macOS the directory is `~/Library/Application Support/BeeperTexts`;
no credentials. Lightly used — expect rough edges. The app's cache is
all it reads, and keeping what that cache lets go is the point of
mirroring it, so nothing is ever deleted from the copy.

### Limitations

#### Limitations

- **The copy holds only what the Beeper Texts desktop app has cached on
  this computer.** Datalib reads the app's data directory and nothing
  else; it never asks Beeper's servers or any chat network for history.
  A chat or a stretch of history the app never loaded is not in the
  copy. Cause: Datalib's own design (the source reads the app's local
  files). Nothing in the config changes it; scrolling back in the app
  makes it cache more.
- **There is no `since`: everything the cache holds is mirrored, from
  every network.** The render step's `networks` list picks which
  networks become documents (every network when unset); the download
  copies all of them either way. Cause: Datalib's implementation.
- **iMessage chats are not here, even when Beeper shows them.** The app
  reads iMessage live from `~/Library/Messages/chat.db` and does not
  keep it in its cache. Cause: how the app stores iMessage. Use the
  `apple_messages` source for those.
- **Chats on a network the render does not recognize are left out of
  the documents.** A chat is shown only when its account id names one
  of a fixed list of bridges (Signal, Google Chat, Slack, WhatsApp,
  Telegram, Discord, LinkedIn, Twitter, Instagram, Facebook, Google
  Messages). Every other chat is counted in a problem row
  (`unlisted_bridges:<bridge>`), a warning for an unknown bridge and
  `info` for Beeper's own `hungryserv` and `$space` threads. Its rows are
  still in the raw store. Cause: Datalib's implementation. Nothing in
  the config changes it.
- **Nothing is ever removed from the copy.** A message, chat or person
  the app drops from its cache stays, which is the point of mirroring
  it. A message deleted on its network therefore stays in the copy too,
  unless the app rewrites its row. `when_deleted_upstream` is not
  offered. Cause: Datalib's implementation (the mirror only adds and
  replaces rows). Nothing changes it.
- **A message the app marks as edited or deleted is drawn like any
  other.** The mirror keeps the app's `isEdited` and `isDeleted` columns
  and replaces a row when the app rewrites it, but the render reads
  neither column, so a document shows no "edited" or "deleted" mark.
  What the app writes into a deleted message's row has not been
  checked. Cause: Datalib's implementation. The earlier versions of a
  row are only in the store's history.
- **No message is drawn unread.** The cache has read-state columns
  (`seenByMeAt`, `countsAsUnread`), but what they mean on a real cache
  has not been checked, so render ignores them. Cause: Datalib's
  implementation.
- **Only a Slack chat links back to its network; no message does.** A
  Slack room links to its channel through Slack's `app_redirect`. Other
  networks' chats, and every message, have no link, because the cache
  holds no id that builds one. Cause: the app's cache.
- **An attachment has bytes only if the app had cached the file.** The
  download copies every file under the app's `media/` folder into the
  blob store; an attachment whose file the app never downloaded draws
  as a placeholder. Turning `texts.media` off (on unless set) skips the
  copy, and every attachment draws without bytes. Cause: the app's
  cache.
- **`blob_size_limit_bytes` is not applied: every cached file is copied
  whatever its size.** Cause: Datalib's implementation (the media copy
  never reads the limit).
- **An attachment whose id is not an `mxc://` or `localmxc://` link
  cannot be placed** and is a problem on its chat's document. Cause:
  Datalib's implementation.
- **Every sync copies the whole of the app's `index.db` before reading
  it.** The download takes a `VACUUM INTO` snapshot (a compacted copy)
  in a temporary folder each run, so a run's cost grows with the size
  of the cache even when nothing changed. If the snapshot fails, the
  file is copied as it stands, which can be inconsistent while the app
  is writing to it. Cause: Datalib's implementation.
- **A run stops the copy where the app's database changes how a table
  is keyed.** The mirror cannot re-key rows it already holds, so a
  Beeper update that changes a table's key fails the run until someone
  fixes it. Cause: Datalib's implementation.
- **Matching a Signal or WhatsApp message to the same message in a
  phone backup is not done.** The ids that would join them are kept
  (the bridge's `message` table), but nothing joins them yet, so a
  conversation mirrored both here and by the `signal` or `whatsapp`
  source appears twice. Cause: Datalib's implementation.
- **Only the macOS location is a default.** On another system,
  `texts.path` must name the app's data directory. Cause: Datalib's
  implementation.

### Reading the copy

- **One document per chat** (a Beeper "room"). Grid rows are labelled
  by network: `source_label` is `Beeper:<Network>` (`Beeper:Signal`,
  `Beeper:Google Chat`), so `source_label LIKE 'Beeper:%'` is everything
  from Beeper. The chat's row has kind `<Network> Chat`; a message's
  kind follows Beeper's message type: `<Network> Message` (text and
  notices), `<Network> Image`, `<Network> Video`, `<Network> File`,
  `<Network> Audio`, `<Network> Membership`, `<Network> Hidden` (events
  the app hides, drawn as a one-line note), or `<Network> <TYPE>` for
  any other type. Reactions are `<Network> Reaction` rows. No time
  series, no occurrences.
- **The raw store** is `<data_root>/<group>/ingest/entities.doltlite_db`.
  It holds the app's own tables under the app's own names and columns
  (`threads`, `participants`, `participant_identifiers`,
  `mx_room_messages`, `mx_reactions`, and the on-device bridges'
  `message`), plus `beeper_media_files`.
- **"Me"** is the `participants` row with `is_self = 1` in each chat;
  a message the account sent has `mx_room_messages.isSentByMe = 1`.
- **A chat is assembled** from `mx_room_messages` rows whose `roomID` is
  the chat's `threads.threadID`, in `timestamp` order, with reactions
  from both `mx_reactions` and `mx_room_messages` rows of type
  `REACTION`. A reply names the event it answers in `inReplyToID`.
- A grid row's `upstream_id` is the Matrix id: the event id for a
  message, the thread id for a chat.

## Calendar (CalDAV)

`type = "calendar"` — any CalDAV server, such as iCloud or Nextcloud
(`caldav`), **or** a folder of `.ics` files (`ics`). Mirrors your
events. Calendly, Fastmail and Google have sections of their own:
[Calendly](#calendly), [Fastmail Calendar](#fastmail-calendar),
[Google Calendar](#google-calendar).

Every event becomes a page and a row in the grid, dated by when it
happens, whichever way the calendar was reached. A recurring event is
one page that says how it repeats, which dates were cancelled, and
which were moved or edited; each moved or edited date gets a page of
its own, linked both ways with the series. The dates a series simply
repeats on are not pages: there can be infinitely many of them, and
the rule on the series page is what they are. To ask what is on the
calendar on given days, the `render_occurrences` step lists every
occurrence of every event in a window of days around its last run
(30 back and 400 ahead unless its `days_back` and `days_ahead` say
otherwise), one row each, the way Google's own calendar lists them.

- **A CalDAV server.** Put it in `caldav.server_url` — the host alone
  is usually enough, since discovery tries `/.well-known/caldav`. The
  login is latchkey's: register a service for the server's host
  (`latchkey services register`), then give it an app password with
  `latchkey auth set`.
- **`.ics` files.** Point `ics.path` at a folder of them — Google
  Takeout's `Calendar/` is one. Each file is one calendar. No
  credentials.

A `calendars` list of names narrows a server to those calendars; leave
it out for all of them. Adding a calendar to the list later downloads
it whole. `since` and `until` narrow it to a stretch of days: only
events with some part in it, and only the changed dates of a series
that fall in it. Each is a day (`"2026-09-01"`) or an offset from the
day of the sync (`since = "-2d"`, `until = "+21d"`), which moves with
it. A windowed calendar is listed whole every run, and an event the
window no longer reaches leaves the mirror.

**To sync a calendar often, leave the window out.** Neither Google nor
CalDAV can pick a listing up where the last one stopped once it is
bounded by dates, so a window costs the whole listing every run; a
busy calendar takes seconds and several requests each time. Without
`since` and `until`, a run after the first asks only for what changed.
To see just the days around now, read the `render_occurrences` step's
table, which is windowed already, rather than windowing the download.

### Limitations

Every way in (CalDAV, `.ics`, Fastmail, Google, Calendly):

- **A date a series simply repeats on is not a page and not a grid row.** Only one-off events, series and moved or edited dates are documents; a cancelled date is a line on its series' page. This is Datalib's design, so a series with no end does not produce infinitely many pages. To ask what is on given days, read the `render_occurrences` step's `occurrences` table, which covers 30 days back and 400 ahead of its last run unless `days_back` and `days_ahead` say otherwise.
- **The occurrences table can be days or weeks old.** The `render_occurrences` step only runs when the download stored something new, so on a calendar where nothing changed its window stays where the last change left it. This is how Datalib's step runner decides what to run. Nothing changes it except a change upstream or a reset of the step; read `occurrences_window` before trusting a missing date.
- **Some repeat rules are not expanded into dates.** A rule that repeats hourly, by the minute or by the second, or uses `BYWEEKNO`, is refused by Datalib's own rule reader; so is an event whose time zone name is not in the IANA database, a Windows name or a bare offset (Outlook's `Customized Time Zone`, for example). Such an event is in the `unlisted` table of the occurrences store and in its `problems`, and nothing is guessed. Its page still shows the rule and the wall-clock time, but its grid row has no start. This is a limitation of Datalib's implementation; nothing in the config changes it.
- **A deleted event leaves the copy at the next run.** There is no `when_deleted_upstream` for calendars; the old version stays only in the store's history. Nothing changes this.
- **`since` and `until` are whole days in UTC, not in your own zone.** `since = "2026-09-01"` starts at `2026-09-01T00:00:00Z`, and `until` ends at midnight UTC after the day named. This is Datalib's implementation. An event late in the evening in a zone west of UTC can fall on the other side of the line.
- **A window costs a whole listing every run, and drops what slides out of it.** Neither Google nor CalDAV can resume a listing bounded by dates (Google refuses `syncToken` beside `timeMin`/`timeMax`, and `sync-collection` has no time bound), so a windowed calendar is listed whole each run. Whatever that listing does not name is deleted from the copy, including an event that only moved out of a relative window (`since = "-30d"`). Leave `since` and `until` out to keep everything and get incremental runs.
- **Within a window, a series keeps only the moved or edited dates inside the window.** Its other changed dates are not stored. This is how Google's `timeMin`/`timeMax` and CalDAV's `limit-recurrence-set` answer. Leave the window out to keep them all.
- **A calendar you take out of `calendars` stays in the copy, frozen.** It is no longer downloaded, so edits and deletions to it are not seen, but its stored events keep rendering and showing in search. Only a calendar the account no longer lists at all is deleted. This is an accidental limitation of Datalib's implementation. To drop it, reset the source (`datalib-pipeline reset <data_root> <step>`).
- **A calendar named in `calendars` that the account does not have is skipped**, with a `config:calendars:<name>` problem; if none of the names match, the run fails. This is Datalib's implementation; names match case-insensitively, or by id.

CalDAV servers (iCloud, Nextcloud, and any other):

- **By default every event calendar in your calendar home is mirrored**; a list that holds only tasks is skipped, and so are the scheduling Inbox and Outbox. Calendars shared into another home are reached only if the server lists them in yours. This is Datalib's implementation: it lists the collections of the one `calendar-home-set` the principal names and keeps those whose `supported-calendar-component-set` includes `VEVENT` (or names none).
- **Tasks, journals and other non-event objects are not stored.** An object with no `VEVENT` that has a `UID` is held at its version with a `no_identity` warning on its `dav_resources:` row, and not asked for again until it changes. This is Datalib's implementation.
- **A server without `sync-collection` (RFC 6578) cannot be mirrored unless you set a window.** Without a window the calendar fails with a `listing:calendar <name>` problem; there is no second way to list it. This is Datalib's implementation.
- **On a very large calendar a first sync can take more than one run.** A run follows at most 50 truncated `sync-collection` replies and then stops; the next run carries on from there. Objects the listing named without their data are fetched 100 at a time, and three failed `multiget` requests in a row leave the rest for the next run. These limits are Datalib's implementation; nothing in the config changes them.
- **A deletion is applied only once a whole listing finishes.** When a listing stops part way (`listing:calendar <name>`), nothing it did not reach is deleted until a later run carries it to its end, so an event deleted upstream can stay for a run or more. This is deliberate in Datalib's implementation.
- **Edits are seen at the next run** through the calendar's sync token; a refused or expired token makes that run list the calendar whole.

`.ics` files (`ics.path`):

- **Only files whose name ends in `.ics` are read**, in the folder and every folder under it; each file is one calendar. This is Datalib's implementation.
- **There is no window.** `since` and `until` are not accepted for `ics`; every event in the file is kept.
- **An event with no `UID` is not stored**, and the file gets a `file:` problem saying how many were left out; a file in which no event has a `UID` is refused whole. This is Datalib's implementation: without a `UID` an event cannot be matched to itself from one read to the next.
- **A file that is not UTF-8, has no `BEGIN:VCALENDAR`, or is cut off before its `END:VCALENDAR` is refused**, with a `listing:ics <file>` problem. What an earlier read of it stored is kept, and the next run tries again. This is Datalib's implementation.
- **Only events are read.** A `VTODO` or `VJOURNAL` in the file is ignored. This is Datalib's implementation.
- **Deleting a file deletes its calendar from the copy**, and an event missing from a re-read file is deleted too, since each file is taken as the whole of its calendar.
- **The copy changes only when the file does.** An export is a snapshot; the copy is as old as the newest file you put in the folder.

### Reading the copy

One rendered document per event: a one-off event (grid `kind`
`Event`), a recurring series (`Recurring Event`, its page saying how it
repeats and which dates were cancelled), or one moved or edited date of
a series (`Changed Occurrence`, linked both ways with its series).
Unchanged dates of a series are never documents. A row's `created_at`
and `touched_at` are when the event happens (its start), not when it
was added or edited; `modified_at` is always empty.

For "what is on my calendar on these days", read the
`render_occurrences` step's store,
`<data_root>/<group id>/render_occurrences/occurrences.doltlite_db`:
one row per date in `occurrences`, covering the days in
`occurrences_window`. Sort and compare on `start_utc` and `end_utc`,
never on `start_time`, which is in the event's own zone.

The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`.
Events are in `google_events` (Google), `ics_objects` (CalDAV,
Fastmail and `.ics` files) or `calendly_events` with
`calendly_invitees` (Calendly), and every one of them joins to
`calendars` by `calendar_id`. A Google changed date is its own row
naming its series in `recurring_event_id`; an `ics_objects` row holds
a series and all its changed dates in one iCalendar text.

Whose calendar this is: `accounts.login` (the CalDAV user's address,
Google's primary calendar id, the Calendly user's address). In a Google
payload, the attendee or organizer with `"self": true` is the
signed-in person; in Calendly, the host whose `user` equals
`accounts.principal_href`.

## Calendly

`type = "calendar"` — Calendly's API through latchkey (`calendly`).
Mirrors the meetings people book with you: each one with its hosts,
its invitees and guests, and what every invitee answered on the
booking form. Cancelled meetings are kept, marked cancelled, with who
cancelled and why. Rendered the way [Calendar (CalDAV)](#calendar-caldav)
says, one page per meeting.

Built into latchkey, with no browser sign-in. In Calendly, make a
personal access token (**Integrations** → **API & Webhooks** →
**Personal Access Tokens**) with the `users:read` and
`scheduled_events:read` scopes, copy it, and store it:

```sh
latchkey auth set calendly -H "Authorization: Bearer $(pbpaste)"
```

The `calendly` table needs nothing else. `since` and `until` narrow it
to the meetings that start in that stretch of days, each a day or an
offset from the day of the sync as for [Calendar (CalDAV)](#calendar-caldav);
a meeting that leaves it is dropped from the copy. Every sync
lists the meetings whole, and reads a meeting's invitees again only
when the meeting has changed.

This source was built from Calendly's API reference and has not yet been
run against a real account. If something comes out wrong, please file
an issue.

### Limitations

- **This source has never been run against a real Calendly account.** It was written from Calendly's published API reference, and its test data is made up in that shape. Expect surprises; please file an issue.
- **Only meetings booked with you are mirrored**: the scheduled events Calendly's `GET /scheduled_events?user=<you>` lists for the signed-in user, cancelled ones included (marked `canceled`). Meetings of other people in your Calendly organization are not. This is Calendly's API rule for that filter. There is no `calendars` option.
- **`since` and `until` bound by start time only**: a meeting is kept when it starts within the window, in UTC days. A meeting that leaves the window is deleted from the copy. This is Calendly's `min_start_time`/`max_start_time` filter.
- **Every run lists every meeting in range again**, about one request per 100 meetings. Calendly offers no change token and no "updated since" filter.
- **The first sync costs one more request per meeting**, for its invitees, one at a time. Calendly allows 500 requests a minute on a paid plan and 50 on the free one (from Calendly's reference, not measured), so a free account with thousands of meetings takes the better part of an hour or more. A `429` is waited out for the seconds Calendly names in `X-RateLimit-Reset`.
- **A change to an invitee alone (a no-show mark, a reconfirmation) may never be seen.** A meeting's invitees are read again only when the meeting's own listing entry changes. Whether Calendly changes the entry for such an edit is not known. This is Datalib's implementation, chosen to keep runs cheap; nothing in the config changes it.
- **A meeting whose invitees would not come shows no invitees and no booking answers** until a later run fetches them; it stays owed, with a `calendly_invitees:<id>` problem. Ten failures in a row end the invitee fetching for that run (a `phase:invitees` problem).
- **A meeting's video link may disappear from the copy.** Calendly's reference says conference details are removed six months after a meeting, and the copy stores each meeting as the listing sends it. Not checked against a live account.
- **Who organized a meeting is not Calendly's order.** Hosts are sorted by user, so the page names you as organizer when you host, else the first host in that order. This is Datalib's implementation.
- **A token for a different Calendly user replaces the copy**: the first user's meetings are deleted at the next run.

## ChatGPT

`type = "chatgpt"` — web API through latchkey (`api`), a data export
(`export`), or both. Mirrors your conversations.

A one-time registration, then a browser login. ChatGPT uses a bearer
access token rather than a cookie, and latchkey can go and fetch it
for you. The app's Add Data Source wizard does both from its
**Sign in with browser** button (and **Load conversations** then lists
the account's conversations to pick from); by hand it is:

```sh
latchkey services register chatgpt \
  --base-api-url="https://chatgpt.com/" \
  --login-url="https://chatgpt.com/auth/login" \
  --login-flow=token-capture \
  --login-flow-params='{"tokenUrl": "https://chatgpt.com/api/auth/session", "tokenField": "accessToken"}'
latchkey auth browser chatgpt
```

The second command opens chatgpt.com, waits for you to log in, and
stores the token itself — nothing to copy or paste. The token lasts
about ten days. When it runs out, a sync runs that same `auth browser`
line for you: while the browser is still signed in to chatgpt.com, a
window opens and closes by itself within seconds. If the browser has
been signed out too, the window waits on the sign-in page for two
minutes; sign in there, or run the line yourself later. **Test
connection** does not do this, and says `HTTP 401 token_expired`.

If it still says `token_expired` right after a fresh login, look for
a *second* service on the same address: `latchkey curl` picks the
first registered service whose base URL matches, so an older
registration for `https://chatgpt.com/` carries its credential on every
request. `latchkey services list` shows them; `latchkey services
deregister <name>` removes the stale one. If `latchkey services info
chatgpt` shows `authOptions` without `browser`, it was registered
without the login flow: deregister it, then run the two commands above.

To supply the token by hand instead (a machine with no browser, say),
grab it from a logged-in tab via DevTools → **Console**:

```js
(async () => {
  const r = await fetch('/api/auth/session', { credentials: 'include' });
  const j = await r.json();
  if (!j.accessToken) { console.error('no accessToken:', j); return; }
  console.log('click anywhere on the page to copy the token to clipboard...');
  addEventListener('click', async () => {
    await navigator.clipboard.writeText(j.accessToken);
    console.log('access token copied. Now run the staged latchkey auth set command.');
  }, { once: true });
})();
```

Click anywhere on the page to copy the token, then run:

```sh
latchkey auth set chatgpt -H "Authorization: Bearer $(pbpaste)"
```

As with Claude, the impersonating curl clears Cloudflare, so no
`cf_clearance` cookie is needed.

The API fetches one conversation per request, and chatgpt.com
rate-limits a large account's first sync: each run gets as far as the
limit lets it and the next picks up the rest. To skip that wait, ask
for a data export in ChatGPT's Settings → Data controls → Export data
and give the source an `export` table whose `path` is the downloaded
`.zip` (no need to unzip it). On its own it needs no sign-in, and it
brings the attached files too, though not the tool steps (code
output, file reads, image generation) the API shows.

Better, give one source both an `api` table and the `export` table:
in the wizard, the ChatGPT form's **Start from an export?** question.
The first sync fills in every conversation from the export, with its
files, then the API downloads only what changed after the export was
taken; later syncs skip the export until you point it at a new one.
`since` then limits only what the API keeps fresh. Only the API's
listing deletes a conversation, so an export getting older never costs
you one. Don't later drop the `api` table and keep `export`: an export
alone is a complete snapshot, and reading an older one drops every
conversation it predates.

### Limitations

- **With the `api` method and no other setting, the copy holds every
  conversation of the signed-in account, archived ones included, with
  the files they name.** This is how Datalib's implementation walks
  chatgpt.com: both the plain and the archived conversation listings.
  `since`, `max_listing_pages` and `conv_uuids` narrow it.
- **Only conversations are mirrored.** ChatGPT Projects (their files and
  instructions), custom GPTs, memory, custom instructions and shared
  links are not. A conversation held in a Project or a custom GPT knows
  only its id there, so the page names no Project. This is Datalib's
  implementation: it asks chatgpt.com only for the account, the
  conversation listings, each conversation and each file. Nothing in
  the config changes it.
- **With `since` or `max_listing_pages` set, a conversation deleted on
  chatgpt.com is never deleted from the copy.** A deletion is noticed
  only when a run has walked every page of both listings, and with
  either setting the walk stops early. This is Datalib's
  implementation, so that the pages it never asked for are not taken as
  deleted. Unset both (the default) to have deletions followed; then
  each run lists every page, one request per 100 conversations.
- **Without them, a deletion is followed at the next run.**
  `when_deleted_upstream = "keep_local_copy"` (default
  `"delete_local_copy"`) keeps the deleted copy. With `conv_uuids` set,
  no listing is read and no deletion is noticed.
- **The listing can briefly leave out conversations that still exist,
  and a sync then deletes them from the copy.** Measured once: for about
  fifteen minutes after a conversation was unarchived, the listing named
  104 of 582 conversations. The cause is the API. The deleted rows stay
  in the store's history, and the next sync after the listing recovers
  fetches them again. Nothing in the config prevents it, short of
  `keep_local_copy`.
- **Archiving a conversation does not remove it.** It moves to the
  archived listing, which the download reads too.
- **A conversation is fetched again only when the listing shows it
  updated later than the copy.** A change that does not move the
  listing's `update_time` is not seen. The listing's time follows the
  newest message, and for some conversations trails the conversation's
  own time by up to about two days (measured on one account), which is
  why a copy at or after the listed time counts as current. The cause
  is the API. `refetch_newest_conversations` (unset, so 0) fetches that
  many of the most recently updated conversations on every run anyway.
- **`since` limits which conversations are fetched, and stops the
  listing walk once it pages past the cutoff.** A conversation already
  in the copy stays, and moving `since` earlier fetches the older ones
  on the next run. There is no further cap on history from the API.
- **Only the files a message attaches, and the pictures it shows, are
  downloaded.** A file is downloaded when a message names it in
  `metadata.attachments` or shows it as an image (`image_asset_pointer`),
  generated images included. This is Datalib's implementation.
- **A file chatgpt.com answers "not found" (404 or 410) for is not asked
  for again until its conversation changes.** It is kept as missing,
  with a `not_found` warning. This is Datalib's implementation.
- **`blob_size_limit_bytes` has no effect on this source.** Every file
  is downloaded whatever its size. This is Datalib's implementation.
- **A large account's first sync over the API takes several runs.**
  chatgpt.com rate-limits it; each run gets as far as the limit lets it
  and the next carries on. Conversations are fetched one request at a
  time. The cause is the API's rate limit. Starting from a data export
  (the `export` table beside `api`) skips most of the wait;
  `max_conversations_per_run` (unset, no cap) and
  `seconds_between_requests` (unset, no wait) bound or slow a run.
- **A run that stops at `max_conversations_per_run` still reports
  `ok`.** This is Datalib's implementation.
- **On the page, ChatGPT's file and web citations are dropped, and a
  kind of content the renderer does not know shows nothing.** An inline
  link is kept as a link. A content kind it does not know is an
  `uncovered_type` problem on its message. This is Datalib's render;
  the raw store keeps everything.
- **The page shows the branch you last looked at on chatgpt.com; the
  other versions are folded in where they forked.** The raw store holds
  every branch.
- **A conversation stored from a data export has no tool steps** (code
  and its output, file reads, image generation steps) and no system
  messages, until it changes and the API fetches it whole. The cause is
  the export format: it holds only the user's and the assistant's
  messages. With only an `export` table, they never arrive.
- **An export brings no generated file that only a tool step named.**
  The export holds the file, but the tool message that named it is
  left out, so nothing in the conversation names it. The cause is the
  export format. The API brings it when it next fetches that
  conversation.
- **With only an `export` table, a conversation the export no longer
  holds is deleted from the copy.** An export is treated as the whole
  account. Pointing an `export`-only source at an older export deletes
  every conversation made after it. `when_deleted_upstream =
  "keep_local_copy"` keeps them. Beside an `api` table the export
  deletes nothing; only the API's listing does.
- **An export that has not changed is not read again.** Its files'
  fingerprints are kept, and a run that finds them the same skips it.
  To have it read again, change the file (or point `path` at a new
  export).
- **With only an `export` table, there are no `sync_runs` rows.** This
  is Datalib's implementation.

### Reading the copy

Each conversation is one document, grid `kind` `Chat`. Each message is a
row: `User Input` (what the account typed), `LLM Response` (by the
message's model), `LLM Thinking` (reasoning shown as a summary) and
`Tool Call` (tool and system messages). A conversation held in a ChatGPT
Project or custom GPT sits in a container row of kind `ChatGPT Project`
or `Custom GPT`, which has no name. Nothing is rendered as a time
series or occurrences.

The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`,
with file bytes in `blobs.sqlite` beside it. Conversations are in
`conversations`, the account in `me`.

Two things agents get wrong:

- **"Me"** is every message whose `author.role` is `user`; `me` holds
  the account.
- **A conversation is a tree, not a list.** `payload -> '$.mapping'` is
  an object of nodes keyed by id. Follow each node's `parent` back from
  `current_node` to read the branch the page shows; a conversation
  stored from an export has no `children`, so walk `parent`, never
  `children`. A message id is unique only within its conversation: a
  chat branched into a new one repeats the original's ids.

## Claude

`type = "claude"` — web API through latchkey (`api`), **or** an
unpacked Claude data export on disk (`export`). Mirrors conversations
across every org, and projects. The `api` route also mirrors the Cowork
tasks run in the cloud, each as a conversation; `cowork = false` leaves
them out, and `cowork_session_ids` (ids or `https://claude.ai/cowork/…`
links) fetches just the tasks it names. A task run on your computer is
not read.

`claude.ai` has no official API for your conversations, so the `api`
route uses the session cookie your browser already has. Register the
service once, then stage the cookie command:

```sh
latchkey services register claude-ai --base-api-url="https://claude.ai/"
latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
```

Open [claude.ai](https://claude.ai) in a logged-in tab and copy your
`sessionKey` cookie — it's `HttpOnly`, so read it from DevTools →
**Application** → **Storage** → **Cookies** → `https://claude.ai`, find
the `sessionKey` row, and copy its **Value**. With it on the clipboard,
run the staged `auth set` command. The first-time guide has the same
steps with more hand-holding.

The bundled Chrome-impersonating curl clears Cloudflare's challenge, so
no `cf_clearance` cookie is needed: the `sessionKey` cookie is all it
takes.

If you would rather not store a live session at all, request a data
export from Claude's settings and give the `claude` source an `export`
table pointing at the unpacked folder instead. The export is a complete
snapshot, so a re-exported tree that no longer mentions a conversation
drops it from the store.

### Limitations

- **With the `api` method and no other setting, the copy holds every
  conversation in every organization the signed-in account can chat in,
  every Project with its knowledge documents, and every Cowork task run
  in the cloud.** An organization with no chat (an API-console
  organization) is skipped without a problem row, and one that refuses
  the credential is skipped with a `listing:org:<org uuid>` warning.
  This scope is how Datalib's implementation walks claude.ai.
  `conv_uuids`, `project_uuids`, `projects = false`, `cowork = false`
  and `cowork_session_ids` narrow it.
- **Claude Code sessions run on claude.ai ("Claude Code on the web") are
  not mirrored, and neither are Cowork tasks run on your computer.** The
  task listing asks the sessions API only for sessions tagged
  `cowork-remote`. This is Datalib's implementation. Nothing in the
  config changes it. The `claude_code` source reads Claude Code sessions
  run on this machine.
- **Account settings, memory, styles and connected tools are not
  mirrored.** The download asks claude.ai for the account, its
  organizations, their conversations, Projects, Project documents,
  Cowork tasks and attachment bytes, and nothing else. This is Datalib's
  implementation. Nothing in the config changes it.
- **The account's own record (`users`) is read once and never again.**
  A change of name or email on claude.ai does not reach the copy. This
  is Datalib's implementation: `/account` is asked only while the store
  holds no user. Only a reset (`datalib-pipeline reset`) reads it again.
- **A conversation is fetched again only when claude.ai's listing shows
  a newer `updated_at` for it.** Any change the listing's `updated_at`
  does not reflect is not seen. The cause is the API: the listing's
  `updated_at` is the only change signal there is. `refetch_newest_conversations`
  (unset, so 0) fetches that many of each organization's most recently
  updated conversations on every run anyway, at one request each per
  organization per run.
- **A conversation deleted on claude.ai is deleted from the copy at the
  next run that lists its organization.** The listing of one
  organization is one request that returns the whole list, so a
  conversation it leaves out is taken as deleted. If an organization's
  listing fails or is refused, nothing of it is deleted that run.
  `when_deleted_upstream = "keep_local_copy"` (default
  `"delete_local_copy"`) keeps the deleted copy. With `conv_uuids` set,
  no listing is read, so no deletion is ever noticed.
- **Datalib assumes claude.ai's conversation listing is never cut
  short.** The listing is one request with no paging. If claude.ai ever
  returned part of the list, the conversations missing from it would be
  deleted from the copy (they stay in the store's history). This is
  Datalib's implementation, resting on the API's current behavior.
- **A Project or a knowledge document deleted on claude.ai stays in the
  copy, and keeps rendering.** The project walk only adds and updates
  what the listings return. This is Datalib's implementation. Only a
  reset removes them.
- **An edit to a Project's knowledge document can take up to a day to
  reach the copy.** A Project's documents are listed again when the
  Project's `updated_at` moves, and otherwise once every 24 hours; it is
  not known whether editing a document moves the Project's
  `updated_at`. This is Datalib's implementation. Nothing in the config
  changes the 24 hours.
- **A new organization (a new Team workspace, say) can take up to six
  hours to appear.** The organization list is read at most every six
  hours. This is Datalib's implementation. Nothing in the config
  changes it.
- **`since` limits only which conversations and Cowork tasks are
  fetched; Projects are always fetched whole.** A conversation whose
  listed `updated_at` is before `since` is never fetched. One already in
  the copy stays, and moving `since` earlier fetches the older ones on
  the next run. There is no further cap on history from the API.
- **The uploaded original of a file whose text Claude extracted is not
  in the copy, only the text.** A message's `attachments[]` carry the
  text Claude extracted, and claude.ai keeps no download for them. The
  cause is the API. Nothing can change it. The same is true of a
  Project's knowledge documents: only Claude's text is kept.
- **A picture or file claude.ai answers "not found" (404 or 410) for is
  not asked for again until its conversation changes.** It is kept as
  missing, with a `not_found` warning. On one real account, every
  picture uploaded before a certain month answered 404 for a month and
  then downloaded fine. Some older files Claude's sandbox made answer
  404 every time. This is Datalib's implementation (it trusts the 404).
  Making the conversation be fetched again (`conv_uuids`,
  `refetch_newest_conversations`, or a reset) retries it.
- **`blob_size_limit_bytes` has no effect on this source.** Every file
  is downloaded whatever its size. This is Datalib's implementation.
- **A large account's first sync is slow.** Conversations are fetched
  one request at a time, one after another, and each run stops when
  claude.ai's rate limit is not lifted within the source's retry
  bounds; the next run carries on with what is left. This is the API's
  rate limit, together with Datalib's choice of one request at a time.
  `max_conversations_per_run` (unset, no cap) and
  `seconds_between_requests` (unset, no wait) bound or slow a run.
- **A run that stops at `max_conversations_per_run` still reports
  `ok`.** Its `sync_runs.status` does not say that conversations were
  left for the next run. This is Datalib's implementation.
- **The page of a conversation shows the branch you last looked at on
  claude.ai; the other versions are folded in where they forked.** The
  raw store holds every branch.
- **A Cowork task's page leaves out the app's own events and a
  subagent's inner turns.** They are in the raw store (`cowork_events`).
  This is Datalib's render. Nothing in the config changes it.
- **A knowledge document is cut on its Project's page at 128 KiB.** The
  raw store keeps all of it. The render step's `max_project_doc_bytes`
  (128 KiB unless set) changes the cut, and a re-render backfills.
- **With the `export` method, the copy has no attachment bytes, no
  organization names, no Cowork tasks, and no `sync_runs` rows.** The
  export holds JSON only, and names no organization. The cause is the
  export format. Switching to `api` fills them in for each conversation
  the API fetches again.
- **With the `export` method, a conversation, Project or document that
  a newer export no longer holds is deleted from the copy.** An export
  is treated as a complete snapshot. A part of the export that is
  missing (no `projects/` folder, say) deletes nothing of its kind.
  `when_deleted_upstream = "keep_local_copy"` keeps them. Pointing an
  `export` source at an export older than what the `api` method fetched
  into the same store deletes what the export predates.
- **One source cannot use both the `api` and `export` methods.** The
  config is refused. This is Datalib's implementation: the export's
  deletions would remove what the API fetched. To seed from an export
  and then keep fresh from the API, run the export once, then replace
  the `export` table with an `api` one.

### Reading the copy

Each conversation is one document, grid `kind` `Chat`; each Cowork task
stored as a task is one `Chat` document too; each Project is one
document of kind `Project`, with a `Project Knowledge` section per
knowledge document. Inside a conversation, each message and each
thinking or tool step is a row: `User Input` (what the account typed),
`LLM Response` (by the conversation's model), `LLM Thinking` and `Tool
Call`. A conversation in a Project sits inside that Project. Nothing is
rendered as a time series or occurrences.

The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`,
with attachment bytes in `blobs.sqlite` beside it. Conversations are in
`conversations`, Projects in `projects` and `project_docs`, Cowork tasks
in `cowork_sessions` and `cowork_events`.

Two things agents get wrong:

- **"Me"** is every message whose `sender` is `human`; `users` holds the
  account. A conversation's payload from the API does not name the
  account.
- **A conversation's messages are a tree, not a list.**
  `payload -> '$.chat_messages'` holds every branch. Follow
  `parent_message_uuid` back from `current_leaf_message_uuid` to read
  the branch the page shows. In a payload from the API a message's
  `text` is empty; the words are in its `content[]` blocks of type
  `text`.

## Claude Code (Local)

`type = "claude_code"` — reads the transcripts Claude Code keeps on
this machine (`sessions`). Mirrors every session run in the terminal,
the desktop app or an IDE extension, one document per session and one
per subagent, with tool calls and results folded away.

No credentials and nothing to export: an empty `sessions` table reads
the standard store, `~/.claude/projects`. Give it a `path` for a copy
of that folder from another machine. A session Claude Code later
deletes stays in the mirror.

### Limitations

- **With an empty `sessions` table, the copy holds every session file
  under `~/.claude/projects` on this machine, from the oldest Claude
  Code still keeps.** Every session run in the terminal, the desktop
  app or an IDE extension writes there, and each subagent's transcript
  is a file of its own. This source takes no `since`: everything on
  disk is read. `path` points it at another folder, such as a copy from
  another machine.
- **Sessions Claude Code keeps somewhere else are not read.** The
  `CLAUDE_CONFIG_DIR` variable, which moves Claude Code's folder, is not
  read; set `path` to that folder's `projects` instead. Sessions run on
  claude.ai (Claude Code on the web) are not read either, unless one was
  brought to this machine (`claude --teleport` writes it into the same
  folder). This is Datalib's implementation.
- **A session Claude Code deletes stays in the copy, for good.** Claude
  Code deletes old sessions on its own schedule (`cleanupPeriodDays`);
  outliving that is the point. The cause is Datalib's implementation, on
  purpose. There is no `when_deleted_upstream` setting; only a reset
  (`datalib-pipeline reset`) removes them.
- **A line removed from a session file stays in the copy.** A file that
  changed is read again whole and every line in it written over its
  row, but a row whose line is gone is not removed. Claude Code only
  adds lines to a session file, so this matters only for a file edited
  by hand. This is Datalib's implementation.
- **Only what the person typed, Claude's replies, and the tool calls,
  tool results and system notes are kept: one row per `user`,
  `assistant` or `system` line.** The other lines of a session file are
  summed into counts on the session's row (`record_counts`), and their
  content is not kept, even in the raw store: the `attachment` lines
  (the context Claude Code injects: file snapshots, skill listings,
  reminders), `file-history-snapshot`, `last-prompt` and the rest. The
  title, the cloud-session link and the pull-request links are kept on
  the session's row. This is Datalib's implementation.
- **A tool result Claude Code spilled to a separate file is not read.**
  Claude Code writes an oversized tool result to
  `<session>/tool-results/<id>.txt`, and only `.jsonl` files are read.
  `sessions-index.json` is not read either. This is Datalib's
  implementation.
- **A line that is not JSON, or a content line with no `uuid`, is
  skipped.** One `problems` row per file says how many and where the
  first was. The last line of a file is never counted: a session open
  right now has a half-written last line, and the next sync reads it
  once Claude Code finishes it. Bytes that are not UTF-8 are read as
  U+FFFD (the replacement character) and noted on the same row.
- **If the same record (the same `uuid`) is in two session files, the
  copy keeps it once, under the file read last.** It is not known
  whether Claude Code ever writes one record into two files. This is
  Datalib's implementation: a record's id is unique in the store.
- **Nothing of a sync is visible until the whole sync is done.** Every
  file a run reads is written in one transaction, so a first sync of a
  large folder shows nothing until it finishes. A file that has not
  changed costs only a `stat`. This is Datalib's implementation.
- **Secrets in a session are copied as they are.** A tool result can
  hold the output of `env` or a `.env` file. Nothing is redacted, in the
  raw store or on the page.
- **On the page, every line of a session is in time order, not by
  branch.** When a person went back and edited an earlier prompt, both
  versions appear one after the other. The raw store keeps each line's
  `parentUuid`. This is Datalib's render.
- **On the page, a tool result and a tool call's input are cut at 16
  KiB, and pictures are not shown** (the page says how many there
  were). The raw store keeps them whole. The render step's
  `max_tool_result_bytes` (16 KiB unless set) changes the cut, and a
  re-render backfills. Nothing changes the pictures.
- **A subagent's transcript is its own document**, titled after the
  session it ran in; the page does not link it to the `Agent` call that
  started it.

### Reading the copy

Each session file is one document, grid `kind` `Claude Code Session`: a
session, or one subagent's transcript, which sits inside its session
and is titled `<its title> — subagent of <session title>`. Sessions sit
in a `Claude Code Project` named after the last part of their working
directory. Each line is a row: `User Input` (what the person typed, by
"Me"), `LLM Response` (by the model), `LLM Thinking`, `Tool Call`, `Tool
Result`, `Harness Message` (what Claude Code wrote in the person's
place), `Agent Prompt` (a subagent's brief) and `System`. Nothing is
rendered as a time series or occurrences.

The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`.
`transcripts` has one row per file, `records` one row per kept line.

Two things agents get wrong:

- **"Me"** is a `user` line whose content is text (not a `tool_result`)
  and which Claude Code did not write itself: not `isMeta`, not
  `isCompactSummary`, not `isSidechain`, and with `origin.kind` absent
  or `human`. A `user` line is just as often a tool result.
- **A subagent's lines carry the parent session's `sessionId`.** Group
  by `records.transcript_id`, which is `<session id>#<agent id>` for a
  subagent, never by `payload ->> '$.sessionId'`.

## Codex (Local)

`type = "codex"` — reads the rollouts Codex CLI keeps on this machine
(`sessions`). Mirrors every thread run in the terminal or an IDE, one
document per thread and one per sub-agent thread, with tool calls and
outputs folded away.

No credentials and nothing to export: an empty `sessions` table reads
the standard home, `~/.codex`. Give it a `path` for a copy of that
folder from another machine. A thread Codex later deletes stays in the
mirror.

### Limitations

- **With an empty `sessions` table, the copy holds every rollout file
  under `~/.codex/sessions` and `~/.codex/archived_sessions` on this
  machine, from the oldest Codex still keeps.** A rollout is the file
  Codex writes for one thread, in the terminal, `codex exec` or an IDE;
  a sub-agent thread is a rollout of its own. This source takes no
  `since`: everything on disk is read. `path` points it at another
  Codex home, such as a copy from another machine.
- **A Codex home somewhere else is not read unless `path` names it.**
  The `CODEX_HOME` variable, which moves Codex's home, is not read.
  Tasks run on Codex cloud are not read. This is Datalib's
  implementation.
- **Nothing else in the Codex home is read**: not `history.jsonl` (the
  prompts typed), not `state_5.sqlite` (Codex's own list of threads,
  with their titles and whether each is archived). A thread's title in
  the copy is a sub-agent's nickname, else the first line of the first
  prompt; Codex's archived flag for a thread whose file stayed in
  `sessions/` does not reach the copy.
  This is Datalib's implementation.
- **A rollout Codex deletes stays in the copy, for good.** There is no
  `when_deleted_upstream` setting; only a reset (`datalib-pipeline
  reset`) removes it. A rollout Codex moves from `sessions/` to
  `archived_sessions/` keeps its document. This is Datalib's
  implementation, on purpose.
- **A line removed from a rollout stays in the copy, and a rollout
  rewritten other than by adding lines is mixed up with its old
  version.** Codex gives a line no id of its own, so a line is stored
  under its line number. Codex only adds lines to a rollout, so this
  matters only for a file edited by hand. This is Datalib's
  implementation.
- **If two rollout files carry the same thread id, they are stored as
  one thread, the lines of the file read last written over the other's
  line by line.** It is not known whether Codex ever writes one thread
  into two files. This is Datalib's implementation.
- **A line that is not JSON, or that has no `type`, is skipped.** One
  `problems` row per file says how many and where the first was. The
  last line of a file is never counted: a thread open right now has a
  half-written last line, and the next sync reads it once Codex
  finishes it. Bytes that are not UTF-8 are read as U+FFFD (the
  replacement character) and noted on the same row. A file with no
  `session_meta` line is not a thread and is not stored.
- **Nothing of a sync is visible until the whole sync is done.** Every
  file a run reads is written in one transaction, so a first sync of a
  large home shows nothing until it finishes. A file that has not
  changed costs only a `stat`. This is Datalib's implementation.
- **Reasoning from Codex 0.155 on cannot be read.** Its thinking is
  stored encrypted, with an empty summary, so the page shows nothing
  for it. The cause is the rollout format. Nothing can change it.
- **Secrets in a thread are copied as they are.** A tool output can
  hold the output of `env` or a `.env` file. Nothing is redacted, in the
  raw store or on the page.
- **On the page, a tool output, and anything Codex put under the user
  or developer role (its AGENTS.md, a permissions note), is cut at 16
  KiB, and pictures are not shown** (the page says how many there
  were). The raw store keeps them whole. The render step's
  `max_tool_result_bytes` (16 KiB unless set) changes the cut, and a
  re-render backfills.
- **Through Codex 0.115, telling what the person typed from what Codex
  injected under the user role is a guess when the two differ.** Those
  rollouts tag nothing. The page counts a message as typed when its text
  matches a `user_message` event, or does not start with `<` or
  `# AGENTS.md instructions`. The cause is the rollout format. From
  0.155 every part is tagged and nothing is guessed.
- **The UI events (`event_msg`), the restated context (`world_state`)
  and token counts are kept in the raw store but not shown.**

### Reading the copy

Each rollout is one document, grid `kind` `Codex Thread`. A sub-agent
thread is a document of its own that sits inside its parent thread's,
with the parent's title in its name. Threads sit in a `Codex Project`
named after the last part of their working directory. Rows, in line
order: `User Input` (what the person typed, by "Me"), `LLM Response`
(by the turn's model), `LLM Thinking`, `Tool Call`, `Tool Result`,
`Harness Message` (what Codex put under the user or developer role) and
`System` (a compaction note). Nothing is rendered as a time series or
occurrences.

The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`.
`transcripts` has one row per rollout, `records` one row per line of it,
every kind of line kept.

Two things agents get wrong:

- **"Me"**: a `response_item` of type `message` and role `user` is not
  always the person; Codex puts its AGENTS.md and other notes there too.
  From 0.155, the person's parts are tagged `user.text` in
  `internal_chat_message_metadata_passthrough.content_item_kinds`; before
  that, an `event_msg` of type `user_message` is the one record of what
  was typed.
- **Order is `records.line_no`**, not `id`: `id` is text
  (`<thread id>#<line>`), so `#10` sorts before `#9`.

## Contacts (CardDAV)

`type = "contacts"` — any CardDAV server, such as iCloud or Nextcloud,
through latchkey (`carddav`), **or** local `.vcf` files (`vcf`).
Mirrors your address book. Fastmail and Google have sections of their own:
[Fastmail Contacts](#fastmail-contacts),
[Google Contacts](#google-contacts). This source is a copy of what
the address book says; saying that several handles are one person is
[the contacts app](contacts_app.md), which needs no source.

- **A `.vcf` export.** Most address books export vCards; point
  `vcf.path` at a directory of them. No credentials. Each file is an
  address book, so deleting a `.vcf` deletes its contacts on the next
  sync.
- **A CardDAV server.** Put it in `carddav.server_url` — the host alone
  is usually enough, since discovery tries `/.well-known/carddav`. The
  login is latchkey's: register a service for the server's host
  (`latchkey services register`), then give it an app password with
  `latchkey auth set`.

An `addressbooks` list of names narrows a server to those address books,
matched exactly; leave it out for all of them. The wizard's **Test
connection** lists them to pick from.

### Limitations

#### Limitations

Both ways in (`carddav`, a server; `vcf`, a folder of files):

- **The copy holds every address book unless `addressbooks` names some.**
  With no `addressbooks` list, every address book the server lists
  under your account is mirrored; with one, only the books whose name
  matches exactly, case included. Cause: Datalib's own choice of
  default. A name that matches no book is a `config:addressbooks:<name>`
  problem row, and if no name matches at all the run fails rather than
  mirroring every book.
- **Narrowing `addressbooks` later does not remove what was already
  copied.** A book you take off the list keeps its last-copied contacts
  in the store, and they keep rendering and showing in search; they are
  simply never updated again. Cause: Datalib's implementation (an
  accidental limitation): only a book the server stops listing is
  deleted, and a book left off the list is still listed. Nothing in the
  config changes it; a reset of the source (`datalib-pipeline reset
  <data_root> <source>/ingest`) clears it.
- **Changing `carddav.server_url` to another host leaves the old host's
  contacts in the copy.** Each server's contacts are stored under its
  host name, and only books of the host now configured are ever
  deleted. Cause: Datalib's implementation (accidental). A reset
  clears it.
- **Renaming an address book gives every contact in it a new document.**
  A rendered contact's id is made from its address book's name and the
  card's `UID`, so after a rename the old documents go and new ones
  appear; a link or saved reference to the old document stops working.
  Cause: Datalib's implementation. Nothing changes it.
- **Only some fields reach the rendered page and the grid.** The page
  shows the name, organization, job title, every email, phone number and
  postal address, the note, the photo, the groups and categories the
  card is in, and for Fastmail the creation time. Birthdays,
  anniversaries, nicknames, web sites, instant-message and social
  profiles, related people, and custom fields are not rendered or
  searchable. Only the first `NOTE` and `TITLE`, and the first `ORG`, are
  used. Cause: Datalib's implementation. The whole card is kept in the
  raw store (`contacts.payload`), so these fields can be read there.
- **A photo the card only links to is not copied.** A photo inside the
  card is copied into the source's blob store; a `PHOTO` that is only a
  web address is shown by that address, so it loads from the remote
  server and is gone if the server drops it. A photo in a format a
  browser cannot draw (not PNG, JPEG, GIF or WebP) is kept but not shown.
  Cause: Datalib's implementation. Nothing changes it.
- **There is no history depth and no `since`.** The address book is
  mirrored whole each time. Earlier versions of a card are only in the
  store's commit history, not in the current tables.
- **Deletions are noticed; there is no `when_deleted_upstream`.** A card
  or address book deleted upstream is deleted from the copy on the next
  sync that sees it gone. The source does not offer keeping a local copy
  of what was deleted.
- **vCard 2.1 quoted-printable text is shown still encoded.** A value
  written with `ENCODING=QUOTED-PRINTABLE` (common in old phone exports)
  appears as `=C3=A9…` on the page. Cause: Datalib's implementation; it
  does not decode quoted-printable. Nothing changes it.

From a CardDAV server (`carddav`):

- **A server that does not support `sync-collection` cannot be
  mirrored.** Each address book is listed with WebDAV `sync-collection`
  (RFC 6578) and nothing else; a server that refuses it fails that
  address book, as a `listing:addressbook <name>` problem row. Cause:
  Datalib's implementation; there is no second way to list a book.
  Nothing changes it.
- **A card with no `UID` is not copied.** RFC 6350 requires a `UID`, and
  without one the card has no stable key. It is a `no_identity` warning
  on a `dav_resources:` problem row, and is tried again once the server
  lists it at a new version. Cause: the vCard format plus Datalib's
  choice not to invent a key for a server's card. Nothing changes it.
- **Edits and deletions arrive at the next sync, not before.** Each sync
  asks the server only what changed since the token the last one kept.
  If the server no longer honours that token, the book is listed whole
  again, and whatever that listing does not name is deleted. Cause: the
  `sync-collection` protocol.
- **A very large address book can take several syncs to finish the
  first time.** One run follows at most 50 pages of a `sync-collection`
  reply, and a server that cuts a reply short (HTTP 507) leaves the rest
  for the next run. Until a whole listing finishes, nothing is deleted
  for being missing from it. Cause: Datalib's limit of 50 pages a run,
  and the server's own page size. Cards the listing named without their
  data are fetched 100 at a time; three failed fetches in a row stop
  that for the run (`phase:multiget <name>` problem row) and the rest
  stay owed.
- **Shared and delegated address books may be missed.** Only the address
  books under your account's address-book home (the server's
  `addressbook-home-set`) are listed. A book another account shares with
  you is copied only if the server puts it there. Cause: CardDAV
  discovery as Datalib does it. Nothing changes it.

From `.vcf` files (`vcf`):

- **A card without a `UID` is keyed by its name, so renaming the person
  makes a new contact.** Google's export writes no `UID`. Such a card's
  key is made from its first and last name (or its formatted name), and
  a second card with the same name in the same file gets the next
  number. Renaming the person, or reordering two people of the same name
  in the file, changes which contact is which. A card with no `UID` and
  no name is keyed by its position in the file. Cause: the export
  format. Nothing changes it.
- **Each file is a whole address book.** A card a re-read file no longer
  holds is deleted; deleting the file deletes its contacts; moving a
  file to another subfolder makes it a new address book (and so, by the
  rename rule above, new documents). A file that holds no card at all,
  or whose last card has no `END:VCARD`, is refused and deletes nothing.
  Cause: the file format, which has no envelope saying how many cards it
  holds.
- **A file that is not UTF-8 is not read at all.** It is a `listing:vcf
  <path>` problem row and is tried again on the next sync. Cause:
  Datalib's implementation; it does not convert other character sets.
  Re-export the file as UTF-8.
- **Edits are seen only when the file changes.** A changed file is read
  again whole; an unchanged one costs one file-system check. The copy is
  as current as your last export.

### Reading the copy

- **One document per card.** A person's card renders with grid `kind`
  `Contact`; a group card (vCard 4 `KIND:group`, or Apple's and
  Fastmail's `X-ADDRESSBOOKSERVER-KIND:GROUP`) renders as `Contact
  group`. The grid's `channel` is the address book's name, and each
  contact is its own conversation. Nothing else is rendered: no time
  series, no occurrences.
- **The raw store** is `<data_root>/<source id>/ingest/entities.doltlite_db`.
  The cards are in `contacts`, one row per vCard, the whole card as text
  in `json_extract(payload, '$.vcard')`. Group membership is already
  split out in `contact_group_members`, and labels (`CATEGORIES`, which
  is how Google's export says which labels a contact has) in
  `contact_categories`. Inline photos are in the blob store beside it.
- **There is no "me" here.** An address book does not mark your own card.
  To find which person a handle belongs to across sources, use the
  contacts app ([contacts_app.md](contacts_app.md)), not this store.
- **A group's members are named by `UID`.** `contact_group_members.member`
  is as the card wrote it (`urn:uuid:<UID>`, sometimes `mailto:`);
  `member_id` is the `contacts.id` it points at, which may not exist if
  the member card was deleted. A contact's keys are scoped to its
  address book: the same `UID` in two books is two rows.

## Discord

`type = "discord"` — Discord's API through latchkey (`api`), signed in
as you. Copies your direct messages, and the channels, threads and forum
posts of the servers you name, with the files in them. Each channel,
thread and direct message becomes one document.

**Discord forbids this, so decide whether the risk is worth it.** Its
rules say that automating a personal account outside its bot API is
forbidden and can get the account terminated. A bot, which is the way
Discord allows, cannot read your direct messages or a server whose admin
has not added it. The download only reads, one request at a time, and
asks for nothing in a conversation with nothing new.

Sign in once in the browser; latchkey keeps your session:

```sh
latchkey auth browser discord
```

An empty `api` table copies your direct messages and no server. Name
servers in `servers`, by name or id, or set `all_servers = true`;
`channels` narrows them to channels you name by name, id or pasted link.
A server with years of history is read 100 messages a request, so set
`since` (a day, or an offset from the sync such as `"-90d"`) on a big
one. Each sync after the first reads only the conversations with
something new, and every 6 hours (`refresh_every_minutes`) re-reads the
last week (`refresh_window_days`) of every conversation holding a
message from that week, for edits, reactions and deletions.

Not copied: who reacted (only how many), the nicknames people use on one
server, what you have and have not read, and a direct message you have
closed, until it opens again.

### Limitations

- **With nothing set, the copy holds the account's direct messages and
  no server.** Cause: Datalib's own defaults (`dms`, `threads` and
  `media` are on; no server is named). Name servers in `servers`, by
  name or id, or set `all_servers = true`; `channels` then narrows those
  servers to the channels (or categories) it names. `dm_conversations`
  narrows the DMs; `dms = false` turns them off.
- **A direct message closed in the Discord app is not read while it is
  closed.** Cause: the API. `/users/@me/channels` lists only the DMs open
  in the account's list. When a message reopens one, the next sync reads
  everything it missed. Nothing else changes it.
- **Each copied server costs requests on every sync, even when nothing
  was posted.** Cause: Datalib's implementation. Every sync lists each
  copied server's channels (one request per server); every
  `list_every_hours` (6 unless set) it also asks each server's roles and
  the account's membership (two requests), and lists the threads of each
  text, announcement, forum and media channel (at least one request per
  channel, 25 threads a page). With `all_servers` on an account in many
  big servers this adds up; `threads = false` drops the thread listings.
  Requests go one at a time.
- **A channel the account's roles do not let it read is not copied, and
  no problem says so.** Cause: Datalib's implementation, by design.
  Discord lists every channel of a server to every member; Datalib works
  out from the roles and the channel's permission overwrites whether the
  account has both "View Channel" and "Read Message History", and never
  asks for one that fails. The channel is still in the `channels` table.
- **A new message in an older thread or forum post can arrive up to 6
  hours late.** Cause: Datalib's implementation. A thread is known to
  have news only from the thread listing, which is asked at most every
  `list_every_hours` (6). A thread started since the last sync is read
  on the same sync, because the message that started it names it.
  `list_every_hours` changes it.
- **An edit, a reaction change or a deletion is seen only for messages
  posted within the last 7 days, and up to 6 hours late.** Cause: the
  API (Discord lists no edits or deletions) and Datalib's
  implementation (the refresh window). A sync re-reads the trailing
  `refresh_window_days` (7 unless set; `0` turns it off) of each
  conversation that holds a message that new, at most every
  `refresh_every_minutes` (360 unless set); what the re-read lacks is
  deleted from the copy. A message edited or deleted after it is older
  than the window stays as it was last read. `when_deleted_upstream` is
  not offered.
- **A deleted channel, thread or server stays in the copy as it was.**
  Cause: the API gives no sign of it; the conversation just stops being
  listed. Nothing changes it.
- **Nothing older than `since` is read.** Cause: Datalib's
  implementation. Unset, `since` is the whole history, read 100 messages
  a request, one request at a time, so a big server takes a long time;
  set `since` (for example `"-90d"`) on one.
- **Some content is not captured.** Who reacted (only how many, and
  whether the account did: naming them is a request per emoji per
  message); how far the account has read (Discord tells only its own
  app); the name a person goes by on one server; pins, calls, events,
  and emoji and sticker pictures; the pictures of link previews and
  bots' cards; channel types other than text, announcement, voice and
  stage chat, forums and media channels; a private thread the account
  is not in. File bytes are not fetched with `media = false`, or for a
  file over `common.blob_size_limit_bytes` (no limit unless set), or
  from any host but Discord's own file host.
- **Copying a personal account is against Discord's rules.** Cause:
  Discord's terms forbid automating a user account outside its bot API,
  and can terminate the account; a bot cannot read DMs or servers that
  have not added it. Datalib only sends `GET` requests, one at a time.

### Reading the copy

One rendered document is one conversation: a channel, a thread, a forum
post, or a direct message, read as one transcript. Its grid rows are one
`Discord Chat` row per document, one `Discord Message` row per message
and `Discord Reaction` rows (the account's own reaction, and one row for
everyone else's together). A server and a channel are places the rows
sit in (`Discord Server`, `Discord Channel`); a DM sits in neither. No
time series. The raw store is
`<data_root>/<id>/ingest/entities.doltlite_db`: messages in `messages`
(keyed `<channel id>#<message id padded to 20 digits>`), conversations
in `channels`, people in `users`.

- **Me** is `account.id`; a message is the account's when
  `json_extract(payload, '$.author.id')` equals it.
- **A message's author name is not in the message.** The payload keeps
  only `author.id` (and `bot`); the name is in `users` by that id
  (`global_name`, else `username`). A webhook's message is the
  exception: its `author` is whole, since its name exists only there.
- **A thread or forum post is its own conversation**: a `channels` row
  with `kind` 10, 11 or 12 whose `parent_id` is its channel (or
  forum), and its messages have that thread's id as `channel_id`. A
  reply points at what it answers through
  `$.message_reference.message_id`.

## Email (mbox or JMAP)

`type = "email"` — a Google Takeout `.mbox` on disk (`mbox`), **or** a
JMAP server other than Fastmail through latchkey (`jmap`). Mirrors mail
messages and their attachments. [Gmail](#gmail), [Fastmail](#fastmail)
and [Apple Mail](#apple-mail-local) have sections of their own; this one is the
other two routes into the same source.


**A Takeout `.mbox`.** Google Takeout's **Mail** product is a single
`.mbox` file; see [Google Takeout](#google-takeout) for requesting one.
Point an `mbox` table's `path` at the folder holding the export's
`.zip` or `.tgz` parts as Google sent them (no need to unpack them), or,
unpacked, at the `.mbox`. No credentials. A sync reads the Takeout only
when it, or the settings, changed since the last sync that read it
through.

**Another JMAP server.** Any RFC 8620/8621 server works the way
Fastmail does: register its host as a latchkey service, store the
credential it takes, and use a `jmap` table with its `hostname`.
Fastmail is the one that is built in.

### Limitations

These apply to an `.mbox` file or Takeout read with an `mbox` table.
For a JMAP server other than Fastmail, see the Fastmail section below:
everything there applies, and two more points are at the end of this
list.

- **An mbox is a snapshot: nothing newer than the export is ever in
  the copy.** The file is read again only when it, or the settings,
  change. Cause: the format. What changes it: a new export in its place,
  or the [Gmail](#gmail) method, which can start from the Takeout.
- **A deletion is noticed only when a later read of every file no
  longer finds the message.** If any file failed to read, or a message
  failed to parse, nothing is deleted in that run, and a
  `listing:removed_records` problem counts what is waiting. An empty or
  0-byte file is never read as "the mailbox was emptied". Deleting the
  file is what drops the messages that only it held.
  `when_deleted_upstream` is not offered. Cause: Datalib's
  implementation.
- **Narrowing `only_extract_labels` (unset: every label) deletes
  nothing.** Mail already copied under a label you stopped asking for
  stays. Cause: Datalib's implementation.
- **An mbox that is not from Gmail has no conversations: every message
  is a document of its own.** Threads come only from Gmail's
  `X-GM-THRID` header. `References` and `In-Reply-To` are not used to
  group messages. Cause: Datalib's implementation. Nothing changes it.
- **An mbox that is not from Gmail shows every message as read, with no
  flags.** Read state and stars come only from Gmail's `X-Gmail-Labels`
  header. `Status:`, `X-Status:` and `X-Keywords:` are not read. Cause:
  Datalib's implementation. Nothing changes it.
- **In an mbox that is not from Gmail, two messages with the same
  `Message-ID` are kept as one.** The first one read is kept; the other
  is dropped without a problem row. A Gmail Takeout is keyed by Gmail's
  own id and does not have this problem. Cause: Datalib's
  implementation.
- **The copy does not know your address unless you set it.**
  `mbox.email_address` is unset by default, and the account is named
  after the file (`account_id`, by default the file name in lowercase,
  or `takeout` for a single `.zip`/`.tgz`). Until it is set, `from:me`
  and `to:me` do not find this mail. Cause: the format. What changes it:
  set `mbox.email_address`.
- **There is no `since`, and `blob_size_limit_bytes` is ignored.** The
  whole file is copied, every message whole. Cause: Datalib's
  implementation.
- **Another JMAP server is less tested than Fastmail.** Nothing in the
  test suite replays a real JMAP server's traffic; the JMAP tests use
  hand-written answers in the shapes the standard gives. Cause:
  Datalib's implementation.
- **A JMAP server that does not state `maxObjectsInGet` is read 50
  messages at a time**, which makes a first sync, and every sync after
  the server forgets its change history, slower. Cause: the server
  (RFC 8620 requires it to state the limit). Nothing changes it.

## Facebook

`type = "facebook"` — Facebook's "Download your information" export,
in JSON (`export`). Mirrors posts, photo albums, comments, reactions
and Messenger conversations; friends as contacts; every other file of the export goes to
the raw store.

**Settings & privacy** → **Settings** → **Accounts Center** → **Your
information and permissions** → **Download your information**. Choose
**Facebook**, pick **JSON** as the format (the HTML flavour is not
read), **High** media quality, and the date range you want, then unzip
the files it emails you into one directory:

```sh
unzip ~/Downloads/facebook-*.zip -d ~/backups/Facebook
```

A large account comes as several zips; unzipping them all into the one
directory is right, because they are slices of one tree
(`your_facebook_activity/`, `connections/`, …). Point `export.path` at
that directory. Every JSON file in it becomes a table in the raw store,
every photo or video a record points at is copied into the store, and
the posts, albums, comments, reactions and friends are rendered. Each
export is complete, so re-ingesting a newer one drops what Facebook
stopped including.

### Limitations

- **The copy holds what the export you gave it holds, and nothing newer.**
  Datalib reads a folder of files that Facebook produced once; it never
  contacts Facebook. Cause: the export format (a one-time "Download your
  information" archive). What changes it: request a new export, unzip it
  into the folder `export.path` names, and sync again.
- **There is no `since` option; the date range is the one you chose when
  you requested the export.** Everything in the export is mirrored, from
  every category. Cause: Datalib's implementation (the `export` method
  takes only `path`, and `since` is refused as an unknown key). What
  changes it: choose the date range and categories on Facebook's
  "Download your information" page.
- **A newer export with a narrower date range deletes the older records
  from the copy.** Each export is treated as the complete state of the
  account: a post, comment, reaction, friend or message that a file
  held last time and the same file no longer holds is deleted from the
  raw store on the next sync (it stays in the store's history but no
  longer renders or shows in search). Cause: Datalib's implementation.
  `when_deleted_upstream` is not offered for this source, so nothing
  can keep them except requesting each export with the full date range.
  A whole file or category missing from the newer export is different:
  its table is left as it was.
- **Something you deleted on Facebook disappears from the copy only when
  you ingest an export made after the deletion.** Cause: the export
  format. Nothing makes it sooner.
- **An edited post or comment shows the versions Facebook put in the
  export, matched by text, so a version can land under the wrong post.**
  The export's edit files (`edits_you_made_to_posts.json`,
  `your_comment_edits.json`) do not say which post or comment each
  version belongs to. Render ties each version to the most similar post
  or comment made no later than it, and only when the texts are at
  least 60% alike; a version that matches nothing is shown as a post of
  its own with a note. Cause: the export format; the matching rule is
  Datalib's. Nothing changes it.
- **A post whose text changes between two exports becomes a new
  document, and links to the old one stop working.** Your own posts
  carry no Facebook id in the export, so each is identified by a hash of
  its whole record; any change to the record (an edit, a field Facebook
  adds) gives it a new identity. Cause: the export format gives no id;
  hashing the record is Datalib's choice. Nothing changes it.
- **Comments and reactions are not attached to the post they were left
  on.** All your comments are one document and all your reactions are
  another, oldest first, because a comment record names no post and a
  reaction names only a URL. Cause: the export format. Nothing changes
  it.
- **Two reactions of the same kind made in the same second become one
  reaction in the rendered copy, and a reaction with no time is left
  out.** Render merges the export's two reaction files on (second,
  reaction kind), which also folds together two different reactions
  that happen to match. Cause: Datalib's implementation (an accidental
  limitation); both raw rows are still in the raw store. Nothing changes
  it.
- **A person is only a name.** The export gives no person an id, so two
  friends with the same name are one handle, and an account deleted
  since shows as "Facebook user". In a group conversation with several
  deleted accounts, their messages cannot be told apart. Cause: the
  export format. Nothing changes it.
- **Only part of the export is rendered and searchable.** Posts (yours,
  on other pages, in groups), photo albums, comments, reactions,
  Messenger conversations and friends are rendered. Everything else —
  photos and videos outside posts and albums
  (`your_uncategorized_photos.json`, `your_videos.json`), places, search
  history, ad and login records, settings — is kept in the raw store only
  and is not searchable. Cause: Datalib's implementation. Nothing in the
  config changes it; read those tables from the raw store.
- **Some photos and videos the export mentions are missing.** Facebook
  leaves some media files (often videos) out of the export while its
  JSON still names them; each one is a warning in `problems`. Media
  files the export ships but no JSON record names are not copied at
  all. The HTML version of the export is not read. Cause: the export
  format (missing files); Datalib's implementation (unnamed files, HTML).
  What changes it: request the export in JSON with media included.
- **A field Facebook adds that render does not know is left out of the
  document.** Each one is recorded as a warning in the render step's
  `problems` with its name and shape, and the value stays in the raw
  store. Cause: Datalib's implementation. Nothing in the config changes
  it.
- **Every sync re-reads and re-parses every JSON file of the export.**
  Photos and videos already stored are not read again. On a large
  export this makes each sync take as long as the JSON takes to parse,
  even when nothing changed (nothing is committed in that case). Cause:
  Datalib's implementation. Nothing changes it.

### Reading the copy

The render step makes one document per post (your timeline posts,
posts on other pages, group posts), one per photo album, one per
Messenger conversation, one document holding every comment you made,
one holding every reaction, and one contact per friend. The grid kinds
are `Facebook Post` with `Facebook Post Message` and `Facebook Post
Version` rows; `Facebook Album` with `Facebook Album Message` (the
description) and `Facebook Photo` rows; `Facebook Comments` with
`Facebook Comment` and `Facebook Comment Version`; `Facebook Reactions`
with `Facebook Reaction`; `Facebook Conversation` with `Facebook
Message` (and `Facebook Reaction` for a reaction on a message); and
`Contact` for a friend. A conversation's project says which Messenger
folder Facebook filed it in (`Messenger`, `Messenger · requests`,
`Messenger · filtered`, `Messenger · end-to-end encrypted`, …). It
renders no time series.

The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`,
and every JSON file of the export is a table in it, named after the
file's path (`raw_stores.md` § Facebook). Messenger is in two tables,
`messenger_threads` and `messenger_messages`. "Me" is the profile's
full name: a message is yours when its `sender_name` equals
`profile_v2.name.full_name` in
`personal_information_profile_information_profile_information`. A
message's row id starts with its conversation's id, which is the digits
the conversation's folder name ends in.

## Fastmail

`type = "email"` — Fastmail's JMAP API through latchkey (`jmap`).
Mirrors mail messages and their attachments.

Fastmail is built into latchkey, including the region-prefixed API
hosts (`phl.api.fastmail.com` and the like) that an account homed in a
regional datacenter gets. The browser flow stores an OAuth token:

```sh
latchkey auth browser fastmail
```

That login can read, change and send mail: Fastmail offers no
read-only scope for it. For read-only access, use an API token instead.
Create one at
[app.fastmail.com/settings/security](https://app.fastmail.com/settings/security)
under **Integrations** → **API tokens** → **New API token**, tick
**Read-only access**, copy it, and paste it into the wizard's **Paste a
credential** form — give it an account name of its own there, or it
replaces the browser login latchkey already holds. From a terminal:

```sh
latchkey --account you-readonly auth set fastmail -H "Authorization: Bearer $(pbpaste)"
```

and name the same account in the source.

Use a `jmap` table with `hostname = "api.fastmail.com"`. Fastmail's
contacts and calendars are separate sources — see [Fastmail
Contacts](#fastmail-contacts) and [Fastmail Calendar](#fastmail-calendar).

### Limitations

- **By default every mailbox is copied, with no time limit.** There is
  no `since` for JMAP: setting one is refused. Cause: Datalib's
  implementation. What changes it: `only_extract_labels` (unset: every
  mailbox) narrows it to the mailboxes you name by their full path
  (`Work/Projects`). With the default `whole_threads = true`, a message
  in one of them also brings the rest of its conversation, whatever
  mailbox it is in.
- **A sync after more than about two weeks away lists the whole account
  again.** Fastmail keeps less than two weeks of change history. When it
  can no longer say what changed, the next sync asks for every
  message's envelope again, which took about an hour on a measured
  277,000-message account. Messages already stored are not downloaded
  again. Cause: the service's API (`Email/changes` answers
  `cannotCalculateChanges`). What changes it: syncing at least weekly.
- **Edits are seen on the next sync.** A message's flags, read state
  and mailboxes are read again whenever Fastmail reports the message
  changed. Cause: the API (`Email/changes`). The message itself never
  changes in JMAP.
- **Deletions are seen on the next sync, and the copy then deletes the
  message.** `when_deleted_upstream` is not offered. Cause: Datalib's
  implementation (only `claude` and `chatgpt` offer it).
- **In the raw store, Fastmail rows have no To, Cc, In-Reply-To or
  References columns, and the rendered thread draws no "reply to"
  links between its messages.** The download asks Fastmail for the
  sender, subject and dates but not for those four fields, so
  `emails.to_json`, `emails.cc_json`, `emails.in_reply_to` and
  `emails.references_header` are NULL, and render, which links a reply
  to its parent by `in_reply_to`, finds nothing to link. The rendered
  recipient line is not affected: it is read from the stored message.
  Cause: Datalib's implementation (an accidental gap). What changes it:
  read the headers from the stored `.eml`. Nothing in the settings
  changes it.
- **Muted conversations may not show as muted.** The copy reads JMAP's
  `$muted` keyword, but no Fastmail account tried so far held one, so
  whether Fastmail reports it is not known. Cause: the API (a draft
  standard).
- **The message body arrives after the envelope.** Each sync first
  stores what each message is (sender, subject, mailboxes), then
  downloads the full messages, eight at a time. Until a message's
  `.eml` is stored, its rendered entry shows "(unknown sender)" and no
  body. Twenty download failures in a row stop that phase for the run,
  and the step ends `incomplete`; the next sync downloads the rest.
  Cause: the API (no bulk download). What changes it:
  `jmap.blob_download_concurrency` (8 unless set).
- **A message larger than `blob_size_limit_bytes` (unset: no limit) is
  kept without its body and attachments**, with a warning. Cause:
  Datalib's setting.
- **No ↗ link to Fastmail's web app unless the render step sets
  `outlink_format = "fastmail"`.** Cause: Datalib's implementation (the
  render step does not know the server).

## Fastmail Calendar

`type = "calendar"` — Fastmail's calendars over CalDAV, through latchkey
(`fastmail`). Rendered the way [Calendar (CalDAV)](#calendar-caldav) says.

Fastmail's CalDAV login is built into latchkey as `fastmail-dav`, and
takes an app password (Settings → Privacy & Security → Integrations →
App passwords) — not the OAuth login the mail source uses. Set its
Access to **Calendars (CalDAV)** and tick **Read-only access**, so
Fastmail refuses any change made with it. The combined "DAV
(CardDAV/CalDAV/WebDAV)" access has no read-only option, so a read-only
setup takes one app password for calendars and another for
[contacts](#fastmail-contacts), each stored under its own account name.
The wizard asks for it itself (**Paste a key**: your address and
the app password, stored as `you@fastmail.com calendar`); from a
terminal it is:

```sh
latchkey --account "you@fastmail.com calendar" auth set fastmail-dav -u "you@fastmail.com:$(pbpaste)"
```

and name the same account in the source. The `fastmail` table needs
nothing else; `calendars` narrows it to the calendars you name.

### Limitations

Everything under [Calendar (CalDAV)](#calendar-caldav) applies: Fastmail is
reached over CalDAV at `https://caldav.fastmail.com/dav/`.

- **By default every Fastmail event calendar is mirrored**, without its scheduling Inbox and Outbox. `calendars` narrows it; `since` and `until` are accepted here too, at the cost described under Calendar (CalDAV).
- **A cancelled date of a series is an `EXDATE` on the series, not a cancelled event.** Fastmail writes no `STATUS` on its events (measured on about 5,200 objects), so the copy can say a date is off but not that it was cancelled rather than deleted.
- **Events made in Google keep Google's HTML in their description** (measured on a calendar shared from Google). The page turns it into text; the raw store keeps the HTML.

## Fastmail Contacts

`type = "contacts"` — Fastmail's address books over CardDAV, through
latchkey (`fastmail`). Rendered the way [Contacts (CardDAV)](#contacts-carddav) says.

The same `fastmail-dav` login as [Fastmail Calendar](#fastmail-calendar):
an app password (Settings → Privacy & Security → Integrations → App
passwords), not the OAuth login the mail source uses. Set its Access to
**Contacts (CardDAV)** and tick **Read-only access**: Fastmail then
refuses every change made with it, so the mirror can read your contacts
but never alter them. An API token will not work here, even a read-only
one — Fastmail's CardDAV turns them away with a 401.

The wizard's **Paste a key** form stores it under the name in the
**Fastmail account** box, `you@fastmail.com contacts` unless you type or
pick another. latchkey
keeps one credential per name per service, and Fastmail Contacts and
Fastmail Calendar share the `fastmail-dav` service, so their two app
passwords need two different names. From a terminal:

```sh
latchkey --account "you@fastmail.com contacts" auth set fastmail-dav -u "you@fastmail.com:$(pbpaste)"
```

and name the same account in the source. The `fastmail` table needs
nothing else; `addressbooks` narrows it to the address books you name.

### Limitations

#### Limitations

Everything under [Contacts (CardDAV)](#contacts-carddav) for a CardDAV
server applies; the server is always `https://carddav.fastmail.com/dav/`.

- **The copy holds every Fastmail address book unless `addressbooks`
  names some.** Names are matched exactly against each book's name as
  Fastmail shows it. Narrowing the list later leaves the dropped books'
  contacts in the copy, unchanged, as described above.
- **The creation time is known only for Fastmail.** Fastmail writes a
  `CREATED` line on every card, which becomes the contact's created
  time; other servers and files leave it empty. Cause: the services.

## File inventory (Local)

`type = "file_inventory"` — any directory tree on disk (`fswalk.path`).
Keeps a record of every entry — path, kind, size, blake3 hash;
download-only, nothing is rendered.

Nothing but a path. The scan is incremental, keyed on mtime, size and
inode, so a rescan of a big tree is fast, and it stays read-only
against its input unless you turn `stamp` on. Query the record in the
source's raw store.

### Limitations

- **The copy records what each entry is, not when it changed or who owns it.** A row holds the path, whether it is a file or a symbolic link, its size and a hash of its bytes; there is no modified time, creation time, permission, owner or extended attribute. Cause: Datalib's own implementation (the `files` and `dirs` tables have no such columns; the walk reads those values only to decide whether to re-hash a file). Nothing in the config changes it. The commit history of the store is the only record of when a scan saw a change.
- **Only regular files, folders and symbolic links are recorded.** Sockets, named pipes and device files are skipped without a problem row. Cause: Datalib's own implementation. Nothing changes it.
- **Symbolic links are not followed.** A link is one `files` row with `kind = 'symlink'`; its `symlink_target` is the link's text, its `size` is the length of that text, and its `blake3` hashes the text, not the file it points to. Cause: Datalib's own implementation. Nothing changes it.
- **Hidden files and folders are included.** Unlike the `markdown` source, a name starting with a dot (`.git`, `.DS_Store`) is recorded like any other. The only thing left out by default is the options file `.file_inventory.toml` itself. To leave entries out, list gitignore-style patterns under `ignore` in a `.file_inventory.toml` in the scanned folder (none by default).
- **The scan follows folders onto other disks.** A folder that is a mount point of another volume is walked like any other folder, so a tree that contains a mounted disk records that disk too. Cause: Datalib's own implementation (the walk has no same-device check). Nothing in the config changes it; an `ignore` pattern for the mount point leaves it out.
- **A file or folder whose name is not valid UTF-8 is not recorded.** It becomes an error row in `problems` instead. Cause: Datalib's own implementation (every row is keyed by its path as text). Nothing changes it.
- **Making an `ignore` rule narrower does not bring the newly allowed entries back until their folder changes.** If you remove a pattern from an existing `.file_inventory.toml`, the files it used to hide stay missing from the copy, with no error, until something is added to, removed from or renamed in the folder that holds them. A pattern that is added takes effect on the next scan. Cause: Datalib's own implementation, a known bug pinned by a test (an unchanged folder's list of children is taken from this computer's cache of the last scan). Touching the folder, or deleting the cache file (below), makes the next scan list it again.
- **A pattern in a nested `.file_inventory.toml` is matched from the scan root, not from its own folder.** It applies only inside the folder whose file names it, and an unanchored pattern such as `*.tmp` works as expected there. A pattern with a `/` in it (`/raw/`, `drafts/old`) names a path from the scan root, so written in `photos/.file_inventory.toml` it does not match `photos/raw/`. Cause: Datalib's own implementation, a deliberate simplification of gitignore. Write the path from the scan root (`/photos/raw/`).
- **Deletions are noticed on the next scan, and the row is gone from the current copy.** Every scan walks the whole tree and deletes the rows of entries it did not find. There is no `when_deleted_upstream` option; the deleted row is still readable in the store's earlier commits. Cause: Datalib's own implementation. 
- **An entry that could not be read keeps its last row.** A folder that will not list, or a file that will not open, keeps the row (and, for a folder, every row under it) that the last successful scan wrote, so the copy can be stale there. Each such entry has an error row in `problems`. The sizes and folder hashes of the folders above it are computed without it, so they disagree with its kept row. Cause: Datalib's own implementation, deliberate (otherwise an unreadable folder would look emptied).
- **An unchanged file is not re-read; its hash is taken from this computer's cache.** A file whose size, modified time, change time, inode and device all match the last scan keeps its old hash. The cache is a plain SQLite file outside the data root (`fingerprints.sqlite` under `$DATALIB_CACHE_DIR`, else `$XDG_CACHE_HOME/datalib/`, else `~/Library/Caches/datalib/` on a Mac or `~/.cache/datalib/` elsewhere), shared by the `file_inventory`, `media` and `pdf` sources on this computer. Resetting the step does not clear it. Cause: Datalib's own implementation. Deleting the cache file makes the next scan re-hash every file.
- **A first scan reads every byte of every file.** The scan hashes the full contents of each file it has not seen before, so a first scan of a large disk takes as long as reading the disk. Later scans read only files whose size or times moved. Cause: Datalib's own implementation.
- **A scan cannot be stopped part way, and a scan that is killed keeps nothing.** The walk does not check for a stop request, and the store is committed once, at the end. A scan killed before it finishes leaves the copy as the previous scan left it. Cause: Datalib's own implementation. Nothing changes it.
- **`since` has no effect.** Every scan covers the whole tree, whatever its files' dates. Cause: Datalib's own implementation.
- **`scan_meta.case_sensitive` and `scan_meta.inode_stable` are guesses, not measurements.** `case_sensitive` is 0 on macOS and 1 everywhere else, whatever the volume is formatted as; `inode_stable` is always 1. Cause: Datalib's own implementation (marked as a heuristic in the code). Don't rely on either column.
- **With `stamp = true`, the scan writes into the folder it reads.** Each folder whose `.file_inventory.toml` (or a parent's) sets `stamp_me_with_uuid = true` gets an `[identity]` table with a new id written into that file, which rewrites the file and drops any comments in it. Off unless `stamp` is set (false by default). Copying a stamped folder copies its id, so two folders can carry the same `identity_uuid`.

### Reading the copy

`file_inventory` renders no documents. Its only rows in the grid are the storage report Datalib writes about every source (provider `datalib`). Everything is in the raw store, `<data_root>/<group id>/ingest/entities.doltlite_db`: `files` has one row per file or symbolic link, `dirs` one per folder (the scan root is the row whose `id` is the empty string), and `scan_meta` one row naming the root that was scanned.

- **An `id` is a path relative to the scan root**, with `/` between parts and no leading `/`. The absolute path is `scan_meta.abs_path || '/' || id`.
- **`blake3` is 32 raw bytes, not text.** Compare it as is, or show it with `hex(blake3)`, which prints uppercase hex; `lower(hex(blake3))` matches the hashes other sources store as text.
- **A folder's `blake3` covers everything below it**, by name, kind and content, so two folders with equal `blake3` hold identical trees, even under different names. Its `size` and `entries` count everything below it, not only its direct children.
- **What changed in the last scan** is the store's diff between its last two commits (`dolt_diff_dirs('HEAD^1', 'HEAD')`, then `dolt_diff_files`). A scan that found nothing changed makes no commit, so the newest commit's date is when something last changed, not when the tree was last scanned.

## Garmin

`type = "garmin"` — Garmin Connect's API through latchkey (`api`). Mirrors per-day health metrics (sleep, heart
rate, stress, body battery, HRV, SpO₂, …), weigh-ins, activities with
their original FIT files, devices, records, gear, badges, workouts and
goals. The weigh-ins, each family of daily metrics (sleep, heart rate
and HRV, stress and Body Battery, steps, respiration and SpO₂,
training) and the daily log render as pages with charts; each
activity's page charts its heart rate and speed from its FIT file;
each activity, day, personal record and badge is a row in the grid.

latchkey reaches Garmin through its Garmin plugin, which the Add a
source dialog installs into `~/.latchkey/plugins/garmin` the first time
you sign in there. Sign in with the browser, or import a token folder
the `garth` Python tool wrote (`~/.garth`). The sign-in lasts about a
year. `since` says how far back to mirror: a day (`"2024-01-01"`), or
an offset from the day of the sync (`"-90d"`). The first sync makes one
request per metric per day since `since`, so a long history takes a
while; later syncs re-read only the trailing week.

### Limitations

- **With nothing set, the copy starts a year before the first sync and
  never moves later on its own.** Cause: Datalib's implementation; the
  first run records that start in the store
  (`garmin:default_since` in `sync_scope_state`) and every later run
  starts there. Changes it: `since` (a date or an offset such as
  `"-90d"`); moving it earlier backfills once, moving it later keeps
  what is already stored.
- **By default every one of the twenty per-day metrics is mirrored,
  activity FIT files are fetched, and the per-day wellness FIT bundles
  are not.** Cause: Datalib's defaults. Changes it: `metrics` (a subset
  of the twenty names), `activity_files` (true unless set),
  `wellness_files` (false unless set).
- **The first sync is slow: one request per metric per day, about
  7,300 requests for a year of history.** Cause: Garmin Connect's API
  answers one day of one metric per request, and Datalib sends one
  request at a time. Changes it: a nearer `since`, or fewer `metrics`.
- **A day's numbers are re-read only until 7 days after the day; a
  change Garmin makes after that is never seen.** Cause: Garmin has no
  "what changed since" API, so Datalib re-reads a trailing window and
  then treats a day as final. A watch that syncs more than a week late
  leaves those days stored as they were (often empty) for good.
  Changes it: `refresh_days` (7 unless set); a larger value re-reads
  every settled day once more, then keeps re-reading that many days.
- **An activity is only seen if its start date is within the last 7
  days when a sync runs, once the window has been listed once.** Cause:
  Datalib's implementation: after the first full listing, each run asks
  the activity listing only from `refresh_days` before today. So an
  activity uploaded late with an older start (a watch that synced after
  two weeks offline, a hand-entered activity dated in the past) is never
  mirrored, and a rename, type change or deletion of an activity older
  than that is never seen. Changes it: a larger `refresh_days`; or
  moving `since` earlier, which makes the next run list everything
  from the new start.
- **A deleted activity disappears from the copy only if it started
  within the last week; older deletions are never noticed.** Cause:
  Datalib's implementation, as above. `when_deleted_upstream` is not
  offered.
- **A deleted weigh-in is noticed on the next sync, anywhere inside
  the window; one older than `since` is kept.** Cause: Datalib re-reads
  the whole window of weigh-ins every run. Devices, personal records,
  gear, badges, workouts and goals are re-read whole every run, so a
  deletion there is noticed on the next sync. Nothing changes it.
- **An activity's FIT file is fetched once and never again, even if
  the activity is edited.** Cause: Datalib's implementation (the
  original upload is taken not to change). An activity with no FIT
  (entered by hand) is asked for again only when its listing entry
  changes.
- **A FIT file larger than 256 MiB is not stored.** Cause: Datalib's
  own size limit. The activity is kept without its file, and a warning
  is in `problems`. Nothing changes it.
- **Menstrual cycle, pregnancy, nutrition, lifestyle logging and golf
  are not mirrored, nor per-activity splits, weather or zone breakdowns
  beyond what the activity detail carries.** Cause: Datalib does not
  ask for those endpoints. Nothing changes it.
- **Only active goals are mirrored.** Cause: Datalib asks the goal
  listing for `status=active`. Nothing changes it.
- **Many mirrored metrics are not rendered: minute-by-minute heart
  rate, stress and Body Battery, steps chart, floors, hydration,
  intensity minutes, training status, endurance and hill scores, daily
  events, and the wellness FIT bundles.** Cause: Datalib's render reads
  only the fields in its table (`daily.rs` `FIELDS`); the rest are in
  the raw store only. Nothing changes it.
- **Only `garmin.com` accounts work; a `garmin.cn` account cannot be
  mirrored.** Cause: Datalib's implementation; every request goes to
  `connectapi.garmin.com`.
- **A metric that fails ten days in a row is dropped for the rest of
  that sync.** Cause: Datalib's implementation, to avoid one failing
  request per day of history. The days it skipped are asked for on the
  next sync. Nothing changes it.
- **The account can only be read; nothing is written to Garmin.**

### Reading the copy

The render writes one overview page (weigh-ins with a chart, one
section per device, store counts), one page per family of daily metrics
that has any value (sleep; heart rate and HRV; stress and Body Battery;
steps and activity; respiration and SpO₂; training and fitness), one
daily log with a section per day, one page per activity, and one page
of records. Grid `kind` values: `Garmin Weight` (the overview),
`Garmin Device`, `Garmin Metrics`, `Garmin Daily Log`, `Garmin Day`,
`Garmin Activity` (its type in `channel`), `Garmin Records`,
`Garmin Record`, `Garmin Badge`, `Garmin Gear`, `Garmin Workout`,
`Garmin Goal`.

The `render_time_series` step also writes the numbers as time series,
in `<data_root>/<group>/render_time_series/time_series.doltlite_db`:
one per daily field (by calendar day), weight, body fat and BMI (by
instant), and one per field of each activity's FIT file (heart rate,
speed, position, cadence, power and others, by instant, about that
activity's page). The table `time_series` lists them and can be read
with SQL. The points themselves are compressed blobs in
`time_series_chunks` that SQL cannot decode: read them through the app,
`POST /applet/unified_time_series_index/points` with
`{"ids": [...], "from": t, "to": t, "budget": n}` (the app must be
running; above `budget` points, at most 20,000, the answer is buckets
of min, max and mean). A `day` time series counts `t` in days since
1970-01-01 (`date(t * 86400, 'unixepoch')`); an `instant` one counts
milliseconds.

The raw store is `<data_root>/<group>/ingest/entities.doltlite_db`,
with the FIT files in `blobs.sqlite` beside it. The day's metrics are
in `garmin_daily` (one row per metric per day, Garmin's JSON as it
came), activities in `garmin_activities` and `garmin_activity_details`,
weigh-ins in `garmin_weigh_ins`. Everything in the store is the one
signed-in account's own data. A `garmin_daily` row whose payload is
JSON `null` is a day Garmin had nothing for; a day with no row was
never fetched or failed.

## GitHub

`type = "github"` — web API through latchkey (`api`). Mirrors pull
requests and their review threads.

The app's Add Data Source wizard has a form for it, or sign in from a
terminal. GitHub is built into latchkey; the browser flow creates a
personal access token during login:

```sh
latchkey auth browser github
```

That token can change and delete things as well as read them, and it
expires after 30 days, when you sign in again. For a narrower one, make
a token at <https://github.com/settings/tokens> and paste it into the
wizard's **Paste a key**: a classic token with the `repo` scope, or a
fine-grained one with read access to pull requests on the repositories
you want. In a terminal that is:

```sh
latchkey auth set github -H "Authorization: Bearer $(pbpaste)"
```

### Limitations

- **Only pull requests are mirrored, and only the ones GitHub's search finds under three filters: ones you opened (`author:@me`), ones you commented on (`commenter:@me`), and ones that mention you (`mentions:@me`).** Datalib does not run other searches, such as a review requested from you, an assignment, or a review you submitted. A pull request that matches only one of those is not in the copy. Plain issues, GitHub Discussions and repositories you never touched are never read. The cause is Datalib's own implementation: the three searches are fixed in the code. No option changes them. The one way to add other pull requests is `api.pull_requests`, which names pull requests one by one. While that list is set, nothing is searched.
- **Only github.com is reached.** GitHub Enterprise Server is not supported. The cause is Datalib's own implementation: the API address `https://api.github.com` is fixed in the code. Nothing can change it.
- **There is no `since`: the whole history is mirrored.** The first sync lists every matching pull request back to the start of the account and fetches each one. The cause is Datalib's own implementation: the source has no date option. `api.max_prs` (no cap unless set) spreads the first fetch over several runs. It does not narrow what is mirrored.
- **A first sync of a large account takes hours.** Pull requests are fetched one at a time. Each one costs at least four requests: the pull request, then its issue comments, its reviews and its review comments, 100 to a page. GitHub allows 5,000 requests an hour, so this is at most about 1,250 pull requests an hour. Searches have a separate limit of 30 requests a minute. When GitHub says a limit is reached, the download waits for as long as GitHub says, or a minute. The cause is the service's API (its rate limits) and Datalib's implementation (one request at a time). `common.download_params` decides when the download gives up waiting. It does not make the sync faster. The store can be read from the first checkpoint, and each pull request is either stored whole or not at all.
- **An edit or a new comment is seen only when GitHub moves the pull request's `updated_at`.** Each later sync searches only for pull requests updated since the last search. It fetches a pull request again only when the search reports a newer `updated_at` than the stored copy has. When it does, the pull request and all its comments and reviews are fetched again. The cause is Datalib's own implementation, which trusts GitHub's `updated_at`. In a measurement on a real account, no comment, review or review comment was newer than the `updated_at` the search reported. Nothing changes this.
- **A deleted comment or review may stay in the copy for a long time.** A comment, review or review comment is removed from the copy when a fresh fetch of its pull request no longer lists it. That fetch happens only when the pull request's `updated_at` moves (see above). Whether deleting a comment moves `updated_at` has not been measured. The cause is Datalib's own implementation. `when_deleted_upstream` is not offered.
- **A pull request that GitHub no longer serves is kept as it was last fetched.** This happens when GitHub answers 404 or 410, for example because the repository was deleted or the token lost access. The pull request leaves the list of pull requests to fetch, and no later sync asks for it again. Its copy stays, and is rendered and searched like any other. The cause is Datalib's own implementation. Nothing changes it.
- **Pull requests named in `api.pull_requests` are fetched on every run, whatever the store holds.** While the list is set, no search runs, so pull requests found earlier are not refreshed. A pull request that an earlier run could not fetch whole waits for the next run with an empty list. The cause is Datalib's own implementation.
- **The copy holds the conversation, not the code.** It keeps each pull request's record as `GET /repos/{repo}/pulls/{n}` returns it (title, body, state, branches, head and base commits, labels, requested reviewers) and its three comment lists. It does not keep the diff, the changed files, the commits, CI checks or statuses. Images and files attached in a comment are not downloaded: the text keeps their links. The rendered document does not show reactions or labels. The cause is Datalib's own implementation. Nothing changes it.
- **When a search answers more results than it can return, part of a scope may never be listed.** GitHub's search returns at most 1,000 results. The download then searches again below the oldest result, until the scope is covered. That cannot get past more than 1,000 pull requests that share one `updated_at` to the second. Those are left unlisted, a `listing:search <scope>` warning says so, and each run tries again. The cause is the service's API (the search's 1,000-result cap).
- **A search that GitHub answers with `incomplete_results` counts as not done.** Whatever it listed is kept, and the next run searches the same stretch again. A `listing:search <scope>` row says so. The cause is the service's API.
- **If the source is ever signed in as a second GitHub account, the rendered documents may treat the wrong account as "you".** The store keeps one `self_identity` row per account it has signed in as, and nothing removes a row. Render takes the row with the lowest id. The cause is Datalib's own implementation: an accidental limitation. A reset of the source clears it.

### Reading the copy

One rendered document per pull request, holding its description, reviews, general comments and inline review comments. In the grid, the pull request's own row has `kind = "GitHub PR"` and carries the description, so the description has no row of its own. Each comment gets a row of its own: `"GitHub Review"` for a review, `"GitHub PR Comment"` for a comment in the pull request's conversation, and `"GitHub Review Comment"` for an inline comment on the diff. Every row of one pull request shares its `conversation_uuid`. Nothing is rendered as a time series or as occurrences.

The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`. `pull_requests` holds one row per pull request. Its comments are in `issue_comments`, `pr_reviews` and `pr_review_comments`, each joined to it on `(repo_full_name, pr_number)`.

To tell "you" from others, use the `self_identity` row. Its `login` is the account that synced, and each comment's `user_login` is its author. A review comment's `pull_request_review_id` and `in_reply_to_id` are integers, but every table's `id` is text. Compare them with a `CAST`, as in the queries in the raw store section.

## GitLab

`type = "gitlab"` — web API through latchkey (`api`). Mirrors merge
requests and their discussions.

GitLab is built into latchkey too, but takes a personal access token
you create yourself (User settings → Access tokens, with API read
access):

```sh
latchkey auth set gitlab -H "PRIVATE-TOKEN: $(pbpaste)"
```

### Limitations

- **Only merge requests are mirrored, and only the ones you created, are assigned to, or are a reviewer on.** GitLab's merge request list can only filter by author, assignee and reviewer. Datalib runs one list for each of these three. It does not read your to-do items. A merge request where you only commented or were @mentioned is therefore not in the copy. Issues, epics, snippets and wiki pages are never read. The cause is the service's API (GitLab has no "commented by" or "mentions" filter on `GET /merge_requests`) and Datalib's own implementation (the three lists are fixed in the code). No option changes them. The one way to add other merge requests is `api.merge_requests`, which names merge requests one by one. While that list is set, no list is run.
- **Only gitlab.com is reached; a self-managed GitLab is not supported.** The cause is Datalib's own implementation. The API address `https://gitlab.com/api/v4` is fixed in the code, and a merge request whose address is not on `https://gitlab.com/` is ignored. Nothing can change it.
- **There is no `since`: the whole history is mirrored.** The first sync lists every matching merge request back to the start of the account and fetches each one. The cause is Datalib's own implementation: the source has no date option. `api.max_mrs` (no cap unless set) spreads the first fetch over several runs. It does not narrow what is mirrored.
- **Merge requests are fetched one at a time, two requests each.** Each merge request costs one request for its record and one per 100 of its discussions. A large first sync therefore takes a while. When GitLab answers 429 (rate limited) or a 5xx error, the download waits for as long as GitLab's `Retry-After` header says, or backs off on its own when there is none. The cause is Datalib's own implementation (one request at a time) and the service's rate limits. `common.download_params` decides when the download gives up waiting.
- **An edit or a new comment is seen only when GitLab moves the merge request's `updated_at`.** Each later sync lists only merge requests updated since the last list. It fetches a merge request again only when the list reports a newer `updated_at` than the stored copy has. When it does, the merge request and all its discussions are fetched again. The cause is Datalib's own implementation, which trusts GitLab's `updated_at`. No one has measured which GitLab actions move it. Nothing changes this.
- **A deleted comment or thread is removed only on the next fetch of its merge request.** The copy drops a discussion that a fresh fetch no longer lists, and a deleted note leaves its discussion's stored copy. Both happen only when the merge request is fetched again (see above). The cause is Datalib's own implementation. `when_deleted_upstream` is not offered.
- **A merge request that GitLab no longer serves is kept as it was last fetched.** This happens when GitLab answers 404 or 410, for example because the project was deleted or the token lost access. The merge request leaves the list of merge requests to fetch, and no later sync asks for it again. The cause is Datalib's own implementation.
- **GitLab's system notes are stored but not rendered.** These include approvals, pushed commits, label and draft changes, and "resolved" events. The raw store keeps every note GitLab returns. The rendered document leaves out every note GitLab marks `system`, so an approval does not show there. The cause is Datalib's own implementation (a deliberate choice: those notes are GitLab's audit log). To see them, read the raw store.
- **The copy holds the conversation, not the code.** It keeps each merge request's record as `GET /projects/{id}/merge_requests/{iid}` returns it and its discussions. It does not keep the diff, the changed files, the commits, pipelines or the approval state. Images and files uploaded in a comment are not downloaded: the text keeps their links. Unsubmitted draft review comments are not read. The cause is Datalib's own implementation (only those two endpoints are read). Nothing changes it.
- **Avatar links are not stored exactly as sent.** A numeric `v=` parameter is removed from every `avatar_url`, so that a changed avatar version alone is not counted as a change. The `local_time` field of your own user record is moved to `self_identity_bookkeeping.volatile_payload`. The cause is Datalib's own implementation (deliberate).
- **If the source is ever signed in as a second GitLab account, the rendered documents may treat the wrong account as "you".** The store keeps one `self_identity` row per account it has signed in as, and nothing removes a row. Render takes the row with the lowest id. The cause is Datalib's own implementation: an accidental limitation. A reset of the source clears it.

### Reading the copy

One rendered document per merge request, with its description and its discussions unrolled into separate notes. In the grid, the merge request's own row has `kind = "GitLab MR"` and carries the description. Each note a person wrote gets a row of its own: `"GitLab Inline Note"` for a note on the diff, and `"GitLab Discussion Note"` for any other note. Every note after the first in a discussion is a reply to that first note. Every row of one merge request shares its `conversation_uuid`. Nothing is rendered as a time series or as occurrences.

The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`. `merge_requests` holds one row per merge request. Its notes are not in a table of their own. Each `discussions` row is one thread, and the thread's notes are in the row's `payload` under `notes`, joined to the merge request on `(project_full_path, mr_iid)`. Use `json_each(payload, '$.notes')` to read them one by one, and skip notes whose `system` is true to get what a person wrote.

To tell "you" from others, use the `self_identity` row. Its `username` is the account that synced, and each note's `author.username` in the payload is its author.

## Gmail

`type = "email"` — the Gmail API through latchkey (`gmail`). Mirrors
mail messages and their attachments, with deletions and label changes
as events.

The least setup of any web source. Gmail is built into latchkey; one
command opens a browser, you sign in and approve every scope it asks
for, and latchkey keeps the OAuth token:

```sh
latchkey auth browser google-gmail
```

The first time, latchkey needs an OAuth client to sign in with:
[your own Google OAuth client](#your-own-google-oauth-client).

Use a `gmail` table on the ingest step. Incremental sync is driven by
Gmail's own change history, so deletions and label changes show up as
events. The download stays under Google's per-minute quota, about 80
messages a minute (`quota_units_per_minute`), and a slow link or large
messages bring it lower, so a large mailbox
backfills over several runs. With a Takeout
of the same account, the first sync downloads only what is newer than
it: put the folder holding the Takeout's `.zip` or `.tgz` parts (or,
unpacked, its `Mail` folder) beside the `gmail` table,

```toml fragment
[steps.params.mbox]
path = "~/Downloads/gmail-takeout"
```

or answer "Start from a Takeout?" in the Gmail form. The Takeout alone
is the no-credentials route — see [Email](#email-mbox-or-jmap).

To mirror only recent mail, set `since` in the `gmail` table: a day
(`"2026-09-01"`), an RFC 3339 instant, or an offset from the sync
(`"-30d"`); unset is every message. The first sync
then lists only the mail after it, which takes minutes where a whole
mailbox takes hours. Mail older than a sliding `since` stays mirrored,
and an earlier `since` brings the older mail in on the next sync.

### Different labels to different depths

One `since` has to suit the whole source. To fetch each part of the
mailbox as far back as it is useful, say all of the Inbox but only 90 days
of Sent, write `fetch` sections in the config. **The Add source form cannot
write these; see below.**

```toml fragment
[[steps.params.gmail.fetch]]
labels = ["Inbox"]

[[steps.params.gmail.fetch]]
labels = ["Sent"]
since = "-90d"
whole_threads = false
```

Each section takes three settings, all optional:

| setting | what it means | unset |
|---|---|---|
| `labels` | the labels this section takes, named as `only_extract_labels` names them | the whole mailbox |
| `since` | the oldest mail this section takes, written as `since` above | all of it |
| `whole_threads` | whether a message under these labels brings the rest of its thread, however old | true |

What is fetched is the **union** of the sections:

- **They only add.** Their order does not matter, and adding one never
  loses mail another reaches.
- **A label in two sections goes as far back as either asks**, and a section
  with no `since` goes furthest. A message that carries labels from two
  sections comes in if either section reaches it.
- **A label that does not exist takes nothing.** It shows as a problem on the
  source, and the other sections still run. If no label in any section
  exists, the sync fails rather than mirror the whole mailbox.
- **`since` bounds the listing, not what happens later.** New mail under a
  label always comes in. So does an older message that is relabeled, whatever
  its age, and mail a sliding `since` moves past stays mirrored.
- **An earlier `since` in one section walks only that section's labels on the
  next sync**, and fetches only the mail it newly reaches.

`fetch` replaces `only_extract_labels`, `whole_threads` and `since` in the
`gmail` table, which say the same thing for one section. A source that sets
`fetch` beside any of them does not load, and the error names the keys to
move. Moving them into a section fetches nothing again.

**The Add source form does not support sections.** Add them by editing the
config, either in the Manage tab's config editor or in the file. Reopening
such a source in the form is safe with care:

- The form lists `fetch` among the settings it has no field for, and saving
  keeps your sections exactly as written.
- It has no way to show them, so its "Which mail?" and "How far back?" answers
  read "The whole account" and "Everything" whatever your sections say.
- **Do not choose labels or a start date there.** That writes the older keys
  beside `fetch`, and the source stops loading until you remove one or the
  other.

Only Gmail has sections. Fastmail and other JMAP mail, and an `.mbox` on its
own, take `only_extract_labels` and `whole_threads` for the whole source.

### Your own Google OAuth client

Every Google source signs in through an OAuth client in a Google Cloud
project of your own, and latchkey needs one for each Google service
before its first sign-in. In the Add source form, **Sign in with
browser** says when one is missing and offers to create it or to take
one you made; for Google Drive it takes only one you made, and keeps it
for all four of Drive's services. From a terminal, latchkey can create one, which takes a
few minutes and a Cloud project per service:

```sh
latchkey auth browser-prepare google-gmail
```

Or make one yourself and give it to every Google service at once: in
the Google Cloud console, a project with the Gmail, Google Calendar,
Google People, Google Drive, Google Docs, Google Sheets and Google Slides APIs turned
on, and in it an OAuth client of type **Desktop app**. Then, with its
id and secret:

```sh
for s in google-gmail google-calendar google-people google-drive google-docs google-sheets google-slides; do
  latchkey auth prepare "$s" '{"clientId":"<id>","clientSecret":"<secret>"}'
done
```

Each service is still signed in to on its own, since latchkey asks for
each one's permissions separately.

### Limitations

- **By default the whole mailbox is copied, Spam and Trash included,
  with no time limit.** What changes it: `since` (unset: everything),
  `only_extract_labels` (unset: every label), or `fetch` sections, which
  give each label its own `since` (the Add source form cannot write
  those). Cause: Datalib's choice; the API is asked with
  `includeSpamTrash=true`.
- **`since` limits only the listing; older mail still arrives.** A
  message in a conversation that also has newer mail is fetched whatever
  its age (with `whole_threads = true`, the default, under a label
  filter), and an old message that is relabeled is fetched again. Mail
  that a sliding `since` (`"-30d"`) leaves behind stays in the copy.
  Cause: Datalib's implementation, on top of the API (`history.list`
  covers the whole mailbox and has no date filter).
- **A first sync of a large mailbox takes hours.** At the default
  limit of 5000 quota units a minute, Gmail serves about 80 messages a
  minute, so 100,000 messages take about 20 hours, spread over several
  runs. Cause: the service's API (Gmail's per-user quota: 6000 units a
  minute, 60 per full message). What changes it: a Takeout of the same
  account beside the `gmail` table (only the mail newer than it is
  downloaded), or `since`. Raising `quota_units_per_minute` above 6000
  does not help.
- **Label and read-state changes are seen on the next sync**, at 20
  quota units per changed message. Cause: the API (`history.list`
  reports them).
- **After more than about a week without a sync, changes made in that
  gap are missed.** Google keeps change history for "typically at least
  one week". Past that, the next sync starts listing again but keeps
  what it already holds as it is, so a label or read-state change made
  during the gap is not seen until that message changes again. Cause:
  the API's history retention, and Datalib's choice not to fetch every
  message's labels again. Nothing changes it except syncing at least
  weekly.
- **With `since` or a label filter, a message deleted during such a gap
  stays in the copy for good.** Only a listing of the whole account
  with no `since` and no label filter can find messages that are gone
  without a change record. Cause: Datalib's implementation. What changes
  it: nothing short of resetting the step (`datalib-pipeline reset`).
- **Other deletions are seen on the next sync.** A message deleted
  forever in Gmail leaves the copy. Moving a message to Trash is only a
  label change: it stays, filed under Trash, until Gmail empties the
  Trash. `when_deleted_upstream` is not offered. Cause: the API
  (`history.list` `messagesDeleted`); Datalib's implementation for the
  option.
- **Muted conversations are asked for on every sync**, and a mute or
  unmute shows on the next sync. If that search fails, a message fetched
  in that sync has no muted mark until the next one. Cause: the API
  (muting is not a label; it is the search `is:muted`).
- **A message larger than `blob_size_limit_bytes` (unset: no limit) is
  kept without its body and attachments**, with a warning, and is
  fetched again once it fits under the limit. Cause: Datalib's setting.
- **A spent daily quota, a refused sign-in or a retry loop that gives
  up stops the fetch for that run.** What was fetched is kept, the step
  ends `incomplete`, and the next sync continues. Cause: the API's
  limits.
- **The same Gmail account read through Apple Mail is a separate copy.**
  See Apple Mail (Local).
- **No ↗ link into Gmail unless the render step sets
  `outlink_format = "gmail"`.** The link searches Gmail for the
  message's `Message-ID` in the first Google account the browser is
  signed in to (`u/0`). Cause: Datalib's implementation.

## Google Calendar

`type = "calendar"` — the Google Calendar API through latchkey
(`google`). Mirrors every calendar on the account's list, subscribed
ones included. Rendered the way [Calendar (CalDAV)](#calendar-caldav) says.

Built into latchkey; one command opens a browser, you sign in and
approve every scope it asks for:

```sh
latchkey auth browser google-calendar
```

The first time, latchkey needs an OAuth client to sign in with:
[your own Google OAuth client](#your-own-google-oauth-client).

The `google` table needs nothing else; `calendars` narrows it to the
calendars you name, and `since` and `until` to a stretch of days, at
the cost [Calendar (CalDAV)](#calendar-caldav) describes. Behind a latchkey gateway, its permissions have to
allow reading the calendar list (`/users/me/calendarList`) as well as
the events: every sync reads the list first.

### Limitations

Everything under [Calendar (CalDAV)](#calendar-caldav) that applies to every way in applies here too.

- **By default every calendar on the account's calendar list is mirrored**, including subscribed ones and ones you hid in Google Calendar. Datalib asks for `/users/me/calendarList?showHidden=true`. `calendars` narrows it.
- **A calendar you remove from your Google calendar list is deleted from the copy** at the next run, with all its events. Datalib reads the calendar list every run, and treats a calendar that is no longer on it as gone.
- **A cancelled one-off event or a deleted series leaves the copy**, with every moved or edited date of it. A cancelled single date of a series is kept, marked `cancelled`, since that is how the series says the date is off. This follows Google's `showDeleted=true` listing.
- **A series that never ends lists at most 730 dates in the occurrences table.** Google's own expanded listing stops there, and Datalib does the same, so a daily meeting begun in 2020 has no dates after mid-2022 in `occurrences`. A CalDAV or `.ics` series has no such limit.
- **Attachments are links, not files.** An event's attached Drive files are recorded as Google sends them (title and link); the files themselves are not downloaded. This is Google's event resource.
- **When Google expires the sync token, that run lists the calendar whole** (a `410`), and anything the new listing does not name is deleted.
- **An event Google lists without an id cannot be stored**, and while one is in the listing nothing is deleted from that calendar, its sync token stays where it was, and a `listing:calendar <name>` problem says so; the next run asks again. None was seen in about 7,500 events; this is Datalib's implementation.

## Google Chat (Takeout)

`type = "google_takeout"` — the **Chat** product of a Google Takeout
export (`export`, with `google_chat = true`). Mirrors your Google Chat
DMs, spaces and bots with their attachments, rendered to markdown.

Request a Takeout with **Chat** ticked — see
[Google Takeout](#google-takeout), which is the source this is one
feed of. The Add source form ticks it; in a hand-written config, set
`google_chat = true` beside `export.path`.

### Limitations

- **A deleted message leaves the copy only when a newer export
  arrives**, and then it is deleted, with its attachments, whatever
  `when_deleted_upstream` would say (not offered). Cause: the export
  format.
- **An edit is seen, with its earlier versions.** Takeout lists a
  message's `previous_message_versions` and its `updated_date`; the
  render folds the earlier versions in above the message.
- **A quote is shown as a quote, not as a link to the message it
  quotes.** Takeout names the quoted message by its author and text,
  never by its id. Cause: the export format.
- **Reactions have no time.** Takeout gives the emoji and who reacted,
  nothing else. Cause: the export format.
- **A Drive file, link or video a message points at is not
  downloaded.** Only its title and link are kept. Only files Takeout
  wrote beside `messages.json` are stored. Cause: the export format,
  and this source makes no network requests.
- **An attachment the export does not hold is a warning, looked for
  again every run.** It is a `chat_attachments:<message>#<name>` row
  with reason `not_found`, and its `chat_attachments.blake3` stays NULL.
  Cause: the export format (Takeout sometimes names files it did not
  include).
- **Two attachments with the same file name in two conversations,
  read in one sync, can end up with the same bytes.** The bytes read in
  one run are held by file name, so the later file of that name replaces
  the earlier, and both messages' attachment rows point at it. Cause:
  Datalib's implementation (an accidental limitation; not covered by a
  test). Nothing in the config changes it.

## Google Contacts

`type = "contacts"` — your Google contacts through Google's People API
and latchkey (`google`). Mirrors every contact with its labels and
photo. Rendered the way [Contacts (CardDAV)](#contacts-carddav) says: a
label is a page listing who is in it, and a contact names its labels.

Built into latchkey; one command opens a browser, you sign in and
approve every scope it asks for:

```sh
latchkey auth browser google-people
```

The first time, latchkey needs an OAuth client to sign in with:
[your own Google OAuth client](#your-own-google-oauth-client). The
login it makes can change contacts as well as read them; Datalib only
ever reads.

An empty `google` table copies every contact. `labels = ["Bridge crew"]`
copies only the contacts with one of those labels, named as Google
Contacts shows them. A contact that loses the label leaves the copy, and
so does one whose label you take out of the list; a name no label has is
reported on the source and deletes nothing. After the first sync, each run
asks Google only for what changed since the last, and fetches only the
photos that are new. Two lists Google keeps apart are not mirrored:
**Other contacts** (the addresses Google saved because you wrote to
them) and your organization's directory, since the login latchkey makes
has no permission to read either. A contact of a colleague that you
never saved is in the second.

### Limitations

- **Only the account's own "Contacts" list is mirrored.** Other
  contacts (the addresses Google saved because you wrote to them) and
  the organization's directory are not. Cause: the sign-in latchkey
  makes has no permission to read either. Nothing changes it.
- **`labels` narrows what is kept, not what is asked for.** Every
  contact is listed on every full listing, and those with none of the
  labels are dropped. Changing `labels` makes the next run list everyone
  again. A name that matches no label is a `config:labels:` problem and
  deletes nothing; a run where none matches fails. Cause: the People
  API cannot be asked for one label's contacts.
- **An edit or a deletion arrives on the next sync, sometimes one sync
  late.** Each run asks Google what changed since its last sync token,
  and Google's answer can lag a write by some seconds, so a run right
  after an edit may not see it; the next one does. A merge of two
  contacts arrives as one changed and one deleted. Deletions are
  copied; `when_deleted_upstream` is not offered. Cause: the service's
  API.
- **After seven days without a sync, the next one lists every contact
  again.** Google refuses a sync token older than that. It costs one
  full listing, not missing data. Cause: the service's API.
- **Signing in as another Google account drops the first account's
  contacts** on the next run, since each account has its own address
  book. Cause: Datalib's implementation.
- **Only one photo per contact is kept, at 512 pixels**, and a letter
  Google drew in place of a photo is not kept at all. Cause: Datalib's
  implementation.

## Google Drive

`type = "google_drive"` — your Google Docs, Sheets and Slides through
the Drive API and latchkey (`api`). Each Doc renders as one document
with its comment threads folded in after the text they were made on;
Sheets and Slides are kept but not rendered yet. Each sync keeps every
file whose version moved,
whole and in Google's own form, beside its comments, its revision list
and the images it shows. It can also keep a copy LibreOffice opens
(`exports`, below). Older versions stay, and a file you delete from
Drive stays in the backup. Each sync also reads every folder and shared
drive's name, so a Doc sits in the library under its folders, from My
Drive or its shared drive down, and moves when you move or rename them.

The download reaches four Google APIs, and latchkey signs each with its
own service, so it needs all four signed in as the same account. In the
Add source form, **Sign in with browser** signs in to Drive, Docs,
Sheets and Slides one after another; approve every scope each one asks
for. If one is turned down, the button then reads **Finish signing in
as …** and does only the ones left. From a terminal:

```sh
latchkey auth browser google-drive
latchkey auth browser google-docs
latchkey auth browser google-sheets
latchkey auth browser google-slides
```

Each needs an OAuth client first, and one client can serve all four:
[your own Google OAuth client](#your-own-google-oauth-client). The form
asks for one you made and keeps it for all four, rather than creating
one, since latchkey would make a Cloud project for each. If Google
answers that an API is not enabled, turn it on in that client's Cloud
project.

The form asks which files: every file the account can open, only the
ones it owns (`owned_only`), or only the ones you list (`file_ids`, a
link or an id each). `kinds`, `exports` and `revision_exports` are under
Advanced options. `exports` says which copies Drive converts each version to, beside its
native JSON: `["odf"]` for `.odt`, `.ods` and `.odp`, `"ooxml"`,
`"pdf"`, or, written into the config by hand, a list per kind. By
default there are none.
`revision_exports` takes the same values for the revisions Drive lists,
and is the only way to keep versions from before your first sync: each
Doc and Sheet revision is fetched once, as an export. It is off by
default too. A deck's revisions are never fetched, since Google exports
a deck as it is now whatever revision is asked for.

The first sync fetches every file. A later one costs one listing
request per thousand files, plus a few requests per file that changed.
Each version kept costs its native JSON, tens of KB gzipped. An export,
if you turn them on, costs 1–4 MB a version, since Google embeds the
fonts in each, and so does each old revision's; the first sync of a
file with a long history then fetches every revision.

### Limitations

- **By default the copy holds every Google Doc, Sheet and Slides deck the account can open, and nothing else.** That is the files it owns, the files shared with it, and the files in shared drives it belongs to, as long as they are not in the trash. PDFs, images, uploaded Word or Excel files, and other Google file types (Forms, Drawings, Sites, shortcuts) are never copied. Folders are read only for their names and where they sit. Cause: Datalib's implementation lists only the three native types (`files.list` with a `mimeType` filter). `kinds` (default all three), `owned_only` (default `false`) and `file_ids` (default empty, meaning every file) narrow it; nothing widens it.
- **A file named in `file_ids` that is not a Doc, Sheet or deck, or is of a kind `kinds` leaves out, is skipped without any warning.** Cause: Datalib's implementation (an accidental limitation: the kind filter drops it after it is fetched by id, and records nothing).
- **Every sync lists the whole account again.** Drive's `changes` feed is not used, so each sync costs one request per 1,000 files, one per 1,000 folders, one for My Drive's root and one per 100 shared drives, before anything is fetched. Cause: Datalib's implementation.
- **An edit is seen at the next sync, as a new version beside the old ones.** A file is fetched again whenever Drive's `version` for it has moved since the stored copy. Several edits between two syncs become one stored version. Drive also moves `version` with nothing edited, most often for files shared with the account; each such move fetches the file again (measured: 5 of 6 files owed in one sync came back unchanged). Cause: the Drive API's `version` field. Nothing changes it.
- **Versions from before the first sync are not in Google's own form.** Google's Docs, Sheets and Slides APIs return only the current version. With `revision_exports` set (default none), each Doc and Sheet revision Drive lists is fetched once as an export (`odf`, `ooxml` or `pdf`); a deck's revisions never are, because Drive's export link for a deck ignores the revision asked for and exports the deck as it is now (measured). Drive may already have merged or dropped old revisions. Cause: the Google APIs. `revision_exports` is the only way in.
- **Comments, and the revision list, are read only when the file is fetched again.** They are not checked on their own: a new comment or reply reaches the copy at the next sync after the file's `version` moves. Cause: Datalib's implementation. Nothing changes it.
- **A file deleted, trashed or unshared in Drive stays in the copy, and keeps its rendered document.** Nothing in the store says that a file has left Drive. A comment or revision Google later drops also stays; a deleted comment is kept and marked `deleted`. Cause: Datalib's implementation, on purpose: it is a backup. `when_deleted_upstream` is not offered.
- **Only Google Docs are rendered and searchable.** Sheets and decks are kept in the raw store only, counted in one note on the source. Cause: Datalib's implementation.
- **In a rendered Doc some things are left out.** Headers, footers, page and section breaks, and a table of contents are dropped on purpose; fonts, sizes, colours and underline are lost; an equation shows as `(equation)` because the Docs API says where it is and not what it says. Cause: Datalib's implementation for the layout, the Docs API for equations.
- **A comment's author is known by display name only, except the account's own.** Drive gives no address for a comment's author; the account's own comments get its address when the file names the account as an owner, last editor or sharer. Cause: the Drive API.
- **A comment thread on text that was since rewritten or deleted is shown at the end of the Doc**, under "Comments not tied to the text", not where it was made. Drive's anchor can be resolved only by the Docs editor, so the thread is placed by the text it quoted (measured: 2,311 of 2,966 threads placed). Cause: the Drive API.
- **A Doc the account may only view has no suggestions in it.** The Docs API refuses suggestions to a viewer, so the Doc is read without them. Cause: the Docs API.
- **A file the account may only comment on has no revision list,** since Drive lists none to it. Cause: the Drive API.
- **An export can be missing.** Google declines an export over 10 MB, and every export of a file whose owner turned off download for viewers and commenters. The rest of that version is kept and the file carries a warning. Cause: the Drive API.
- **An image larger than 1 GiB in a Doc or deck is not kept.** The rest of the version is kept, with a warning. `blob_size_limit_bytes` is not read by this source. Cause: Datalib's implementation (one plain download stops at 1 GiB).
- **There is no `since`.** Every file is copied whatever its age. Cause: Datalib's implementation.
- **A stored version can be a little newer than its version number.** If a file is edited while it is being fetched, the content is the newer one; the next sync stores the newer version again under its own number. Cause: the Drive API has no way to fetch a numbered version.
- **Exports are large.** Google embeds the document's fonts in each export, so an export costs 1-4 MB a version (measured), and a first sync with `revision_exports` on fetches every listed revision of every Doc and Sheet. Cause: the Drive API.

### Reading the copy

- One rendered document per Google Doc, from its newest stored version: the text as markdown (headings, lists, tables, images linked to the stored bytes, suggestions as `<ins>`/`<del>`), with each comment thread folded in after the paragraph it was made on. Sheets and decks render nothing.
- Grid rows: the Doc has kind `Google Doc`; each comment and each reply has kind `Google Doc Comment`. Each row's `source_url` opens the Doc in Google Docs; a comment's opens that comment.
- Each Doc sits under its folders, named, from `My Drive` or its shared drive down. A Doc in several folders sits in the one with the lowest folder id and is filed under the others.
- No time series and no occurrences.
- The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`, beside `blobs.sqlite`. `files` holds each file's Drive listing entry; `captures` one row per stored version; `capture_parts` points at the bytes (the native JSON, gzipped, the images, the exports) in `blobs.sqlite`; `comments` and `revisions` hang off a file by `file_id`.
- **Google's own content is not in `entities.doltlite_db`.** It is a gzipped JSON blob in `blobs.sqlite`, found through `capture_parts` with `part = 'native.json.gz'`. The newest version is the capture with the highest `CAST(version AS INTEGER)`.
- "Me": a file's `owners`, `lastModifyingUser` or `sharingUser` entry with `"me": true` is the account; a comment's `author.me` likewise.

## Google Takeout

`type = "google_takeout"` — an unpacked Takeout tree on disk
(`export`). Mirrors Google Chat and Voice messages; Maps reviews, saved
places, saved lists and photos, and My Maps maps; YouTube watch history
and subscriptions; Play Books highlights, notes and bookmarks; and
Gemini Apps activity, each prompt with Gemini's response and the files
and images either one carried. Everything is rendered to markdown: a
conversation, and each of Gemini, YouTube and Maps, as one page
(subscriptions are one list), and each saved list, My Maps map and
Play Books book as a page of its own. Voice bills and greetings
are kept in the raw store only.

Self-service export at <https://takeout.google.com>. Deselect all, then
tick just what you want and request a `.zip` (or `.tgz`). Put every part
of the export in a folder of its own and point the source at that
folder; the parts are read as they are, with no need to unpack them.
Keep one export per folder: a folder holding parts of two exports is
refused. An export you have already unpacked works too: point the
source at its `Takeout` folder.

Useful products: **Chat**, **Voice**, **Maps**, **Saved** (Maps'
lists), **My Maps**, **YouTube history**, **Play Books** and
**Gemini** (read by this source), and **Mail**
(a single `.mbox`, read by the [email](#email-mbox-or-jmap) source instead). The
Add source form ticks every product; untick what you want left out. In
a hand-written config every feed is off until its flag beside
`export.path` turns it on (`google_chat = true`, `google_voice = true`,
…); [Google Chat](#google-chat-takeout) and
[Google Voice](#google-voice-takeout) have sections of their own. A Takeout is
a complete snapshot, so it is also the way to notice what Google has
deleted since the last one: put a newer export in its place, and
what it no longer holds leaves the mirror on the next sync. A product
the newer export does not have at all deletes nothing, so an export
requested for one product leaves the others alone. The one thing this
cannot tell apart: a large Takeout comes as several zips, and a product
missing some of them looks smaller, not missing, so keep every part in
the folder (or, unpacking by hand, unpack every part).

### Limitations

- **The copy is only as new as the export in the folder.** Nothing
  asks Google for a new one: each sync reads the export at
  `export.path` and finds it unchanged until the person puts a newer
  export there. Cause: the export format (a Takeout is a one-off
  snapshot you download yourself); this source never reaches the
  network. Nothing in the config changes it.
- **A hand-written config reads nothing until each product is turned
  on.** Every feed switch (`google_chat`, `google_voice`, `gemini_apps`,
  `maps_reviews`, `maps_saved_places`, `maps_saved_lists`,
  `maps_photos`, `my_maps`, `play_books`, `youtube_watch_history`,
  `youtube_subscriptions`) is `false` unless set, and so is
  `google_voice_include_spam`. The Add source form ticks every one,
  spam included. Cause: Datalib's own choice, so a missing key never
  reads a product. Change it by setting the switch to `true`.
- **Much of a Takeout is never read.** Google Keep, Drive, Google
  Photos, Tasks, Chrome, every `My Activity` product except Gemini Apps,
  YouTube comments, playlists and uploads, the Gemini Gems and scheduled
  actions under `Gemini/`, the other folders under `Maps/` (questions
  and answers, followed places, vehicle settings), Voice's
  `Phones.vcf`, and the books themselves beside Play Books' notes. Mail,
  Calendar and Contacts are left to the `email`, `calendar` and
  `contacts` sources. Each kind of file no feed reads is a
  `skipped:unread_files:…` row in `problems`, a warning where no feed of
  this source can read it. Cause: Datalib's implementation (no reader
  exists for those products). Nothing in the config changes it.
- **There is no `since`.** Everything in the export is read, as far
  back as Google put it in the export; nothing older is held back and
  nothing newer can be added. Cause: Datalib's implementation (the
  `export` table has no such key). Nothing changes it.
- **Every attachment in the export is stored, whatever its size.**
  `blob_size_limit_bytes` is not read by this source. Cause: Datalib's
  implementation. Nothing changes it; leave a large product out of the
  export or turn its feed off.
- **A newer export deletes from the copy what it no longer holds,
  including what Google itself discarded.** Put a newer export in the
  folder and every record of a product it holds that is not in it
  leaves the mirror on the next sync: a Chat message you deleted, but
  also a YouTube watch or Gemini prompt that Google's own activity
  auto-delete removed. `when_deleted_upstream` is not offered (the
  config refuses it). Cause: Datalib's implementation (each feed treats
  its file or folder as the whole of its records). The removed rows stay
  in the store's history (`dolt_at_<table>('<older commit>')`); to keep
  them rendered, keep the older export as a source of its own.
- **A product missing from the newer export deletes nothing, but a
  product missing some of its files looks emptied.** An export
  requested without, say, Voice leaves the Voice records alone. A large
  Takeout comes as several `.zip` parts; a part left out of the folder
  is not noticed, and the records only it held are deleted. Cause: the
  export format (the parts do not say how many there are). Keep every
  part in the folder.
- **One folder holds one export.** A folder holding the parts of two
  exports is refused and the sync fails; so is one in which a part
  cannot be read. Cause: Datalib's implementation, on purpose (two
  snapshots read as one would hide what the newer one deleted).
- **Dates written with a time zone this reader does not know are lost,
  and for Gemini the whole feed fails.** Takeout's HTML pages and Chat's
  JSON end each date with the zone's short English name. The reader
  knows a fixed list (the North American, Western/Central/Eastern
  European, British, Moscow, Australian, New Zealand, Japanese and
  Korean names, and any `GMT±h[:mm]`) and refuses a name two zones share
  (`IST`). With any other name a YouTube watch or a Chat message is
  stored with no time, and every Gemini entry is dropped as "no date
  line", which fails the `gemini_apps` feed as a whole (`phase:gemini_apps`).
  Cause: Datalib's implementation. Nothing in the config changes it.
- **An export in a language other than English is not read.** The
  reader looks for the English folder and file names (`Google Chat`,
  `YouTube and YouTube Music/history/watch-history.html`,
  `My Activity/Gemini Apps/MyActivity.html`, …) and parses English
  month names. Cause: Datalib's implementation. Request the export with
  the Google account set to English.
- **Edits show only as a changed record, and some edits replace the
  record.** A Play Books note whose text changed, a Gemini entry, a My
  Maps placemark that moved or was renamed, and a Saved list place whose
  title or link changed are keyed by what they say, so an edit is the old
  row deleted and a new one added, not one record updated. Cause: the
  export format (none of these has an id upstream).
- **A zipped export is unpacked into the system's temporary directory
  on each sync that has something to read.** Only the files of the
  feeds that are on are unpacked, but that needs as much free space as
  those files take. A sync over the same parts with the same feeds on
  reads nothing. Cause: Datalib's implementation.
- **With a zipped export, a file that failed to read is not tried
  again until the parts change.** A run that finished marks the parts
  read, so a file left with a problem (an unreadable Voice record, a
  Chat attachment that would not read) is not looked at again over the
  same parts. A failed feed (`phase:` row) does keep the parts unmarked.
  Cause: Datalib's implementation. Unpack the export into a `Takeout/`
  folder to have every run retry such a file.
- **A Maps review whose link has no place id is not stored.** Reviews
  are keyed by the `!1s…` place id in `google_maps_url` and the date;
  one without either is a `skipped:maps_reviews:…` row. Saved places
  also accept a `cid=` or, for a dropped pin, the `q=` query. Cause:
  Datalib's implementation.
- **Voice bills and voicemail greetings are kept but not rendered.**
  They are in `voice_bills` and `voice_greetings` only. Cause: Datalib's
  implementation.

### Reading the copy

One rendered document per Google Chat space or DM, per Google Voice
conversation, per Saved list, per My Maps map and per Play Books book,
and one each for all of Gemini, all of YouTube watch history, all of
YouTube subscriptions and all of Google Maps (reviews, saved places
and photos together). Grid `kind` values, document first:

| document | its rows |
|---|---|
| `Google Chat` | `Google Chat Message`, `Google Chat Message Version`, `Google Chat Reaction` |
| `Google Voice Conversation` | `Google Voice Message` |
| `Gemini Activity` | `Gemini Prompt`, `Gemini Response` |
| `YouTube History` | `YouTube Watch` |
| `YouTube Subscriptions` | `YouTube Subscription` |
| `Google Maps Activity` | `Google Maps Review`, `Google Maps Saved Place`, `Google Maps Photo` |
| `Google Maps List` | `Google Maps Saved Item`, plus a `Note` or `Description` |
| `My Maps Map` | `My Maps Pin`, `My Maps Line`, `My Maps Shape`, `My Maps Placemark`, plus a `Note` or `Description` |
| `Play Books Book` | `Play Books Note`, `Play Books Highlight`, `Play Books Bookmark`, `Play Books Listening Position`, `Play Books Entry`, plus a `Note` |

It renders no time series. The raw store is
`<data_root>/<group id>/ingest/entities.doltlite_db`, with attachment
bytes in `blobs.sqlite` beside it. Chat is `chat_messages` (with
`chat_groups`, `chat_users`, `chat_attachments`), Voice is
`voice_messages` (with `voice_attachments`), and each other feed has a
table named after it. Things agents get wrong:

- **Who is "me".** In Chat, the account is the address in
  `chat_users.payload -> '$.user.email'`; compare it with
  `chat_messages.sender_email`. In Voice, a text has
  `payload -> '$.is_me'`; a call's `party` is always the other side,
  and `kind` says the direction (`placed`, `received`, `missed`). Every
  Gemini prompt, YouTube watch, Maps entry, list, map and book entry is
  the account's own.
- **Which Chat conversation a message is in.** `chat_messages.group_id`
  is the export's folder name (`DM <id>`, `Space <id>`), which is
  `chat_groups.id`. The space id is also the first `/`-separated part of
  the message id.
- **Times.** `when_ts` keeps the offset it was written in; sort with
  `datetime(when_ts)`. It is NULL where the export's date did not read.

## Google Voice (Takeout)

`type = "google_takeout"` — the **Voice** product of a Google Takeout
export (`export`, with `google_voice = true`). Mirrors your texts,
voicemails and call records, rendered to markdown.

Request a Takeout with **Voice** ticked — see
[Google Takeout](#google-takeout), which is the source this is one
feed of. Set `google_voice = true` beside `export.path`; the spam
folder stays out unless `google_voice_include_spam = true`.

### Limitations

- **The spam folder is left out of a hand-written config.** Set
  `google_voice_include_spam = true` (with `google_voice = true`) to read
  `Voice/Spam/`; the Add source form ticks it. Spam messages render in
  the same conversation as the number's other messages.
- **A conversation is a phone number, or a set of numbers.** Texts and
  calls with one number are one conversation whatever name Takeout
  gives the file. A group text is keyed by its set of numbers, so a
  group whose members change becomes a new conversation. A file with no
  number is keyed by the name in its file name. Cause: the export format
  (Voice has no conversation id) and Datalib's implementation.
- **Only pictures in a text are attached.** A text's attachments are
  the `<img>` files its page shows; any other file Takeout writes beside
  it is not stored and no problem row says so. Cause: Datalib's
  implementation. An attachment the page names and the export does not
  hold is a `skipped:google_voice:…` warning on the file.
- **There are no edits, reactions or read state in Voice.** Takeout's
  Voice pages do not carry them. A deletion upstream is seen only with a
  newer export, and only after the sync has read every Voice file
  cleanly; if one would not read, nothing is deleted that run
  (`listing:removed_records`). Cause: the export format.

## IQAir AirVisual (Local)

`type = "airvisual"` — reads each monitor's own history files
(`export`). Mirrors particulates, AQI, CO₂, temperature, humidity and
VOC as time series, rendered as one page of charts.

An AirVisual Pro (IQAir's indoor air-quality monitor) keeps its history
on the unit — no cloud account is involved — and serves it over a Samba
share: `smb://<ip>/airvisual`,
user `airvisual`, with the password shown on the device under
**Settings › Network › Access Pro data**. Mount the share (in the
Finder, **Go › Connect to Server…**) and point one `devices` entry at
the mount, or at a copy of that folder:

```toml fragment
[[steps.params.export.devices]]
path = "/Volumes/airvisual"
```

Every `YYYYMM_AirVisual_values.txt` under the path is read, the
`archive*/` folders the device starts on each clock change too; a
later run re-reads only the month being written. Each device's
identity is its serial and its name is what it calls itself, both read
from the folder's `latest_config_measurements.json`; set `serial` for a
copy without it, and `name` to override what the device says.

### Limitations

- **Only the monitors listed under `devices` are read, and only from
  their own history files; IQAir's cloud is never asked.** Cause:
  Datalib reads the files the AirVisual Pro serves on its Samba share
  (or a copy of them). Changes it: one `devices` entry per monitor,
  each with its folder's `path`.
- **There is no `since`: every history file in the folder is read, as
  far back as the monitor keeps them.** Cause: Datalib's
  implementation. Nothing changes it.
- **A sync can only read what is mounted at that moment.** Cause: the
  export format: the files live on the monitor. If the share is not
  mounted, that device is skipped with a `listing:device …` problem,
  and what is already stored stays. Changes it: mount the share before
  the sync, or point `path` at a copy.
- **The times in the raw store are the monitor's local clock written as
  if it were UTC, not real times.** Cause: the export format: the Pro's
  `Timestamp` column is its local wall clock read as UTC. Datalib keeps
  it as written in `airvisual_samples.ts_ms` and converts to real time
  only in the rendered page and the time series. Nothing changes it;
  read real times from the time series, or convert yourself (see
  Reading the copy).
- **When the monitor's clock jumps in a way no time zone explains, or
  its time zone is unknown, its readings stay at the time it wrote.**
  Cause: Datalib's clock model needs `history.txt` and a known time
  zone. A `listing:clock <serial>` problem says so. Nothing changes it.
- **A reading of exactly `-1` is stored as missing.** Cause: the export
  format uses `-1` as "no reading", and Datalib treats every `-1` (and
  `-1.0`) as that, so a real temperature of −1 °C is lost. Nothing
  changes it.
- **`Temperature(F)` is not kept, and a column Datalib does not know is
  not stored.** Cause: Datalib's implementation; °F is the °C column
  converted, and an unknown column is named in a `file:…` problem so
  it is noticed. Nothing changes it.
- **The outdoor AQI columns are the public station the monitor
  follows, not this monitor's own reading.** Cause: the export format.
- **Lines logged before the monitor set its clock are kept but not
  placed in time and not rendered.** Cause: the export format: their
  `Timestamp` counts from the boot, not from 1970. They are in
  `airvisual_unplaced_samples`. Nothing changes it.
- **Two lines with the same device timestamp keep only one: the one in
  the file whose path sorts later.** Cause: Datalib's implementation:
  a sample is keyed on the device's own timestamp. This merges a
  `corrupt_` / `restored_` pair of the same month; a `restored_` line
  has fewer columns, so the value only the `corrupt_` line had is lost.
  Nothing changes it.
- **A line cut short by the monitor still writing it costs that line;
  the file is read again next sync.** Cause: the export format (the
  month being written is appended to while it is read).
- **A history file or reading removed from the share stays in the
  copy.** Cause: Datalib's implementation: files are only ever added or
  re-read, never forgotten. `when_deleted_upstream` is not offered.
- **A device taken out of the config is still shown with its old
  readings.** Cause: Datalib's implementation: device rows are never
  removed. Nothing changes it.
- **The current month's file is re-read whole on every sync; over a
  slow Wi-Fi share a first sync takes minutes.** Cause: the export
  format (one file per month, appended to) and the share's speed
  (measured: 3 minutes cold for two monitors, mostly hashing 24 MB over
  Wi-Fi). Only changed files are read again.

### Reading the copy

The render writes one page for the whole source: a chart per quantity
(particulates, AQI, CO₂, temperature, humidity, VOC) and a section per
monitor. Grid `kind` values: `Sensor Timeseries` (the page) and
`Sensor Device` (one row per monitor, its name in `channel`).

The `render_time_series` step writes one time series per monitor and
column, named `<serial>/<column>` (for example `<serial>/co2_ppm`),
by instant in **real** Unix milliseconds, into
`<data_root>/<group>/render_time_series/time_series.doltlite_db`. Its
`time_series` table can be read with SQL; the points are compressed in
`time_series_chunks` and are read through the app,
`POST /applet/unified_time_series_index/points` (the app must be
running).

The raw store is `<data_root>/<group>/ingest/entities.doltlite_db`.
Readings are in `airvisual_samples`, one row per monitor and logged
second, a column per measurement; `device_id` is the monitor's serial,
the same as `airvisual_devices.id`. **`ts_ms` is not a real time**: it
is the monitor's local clock written as UTC, so
`datetime(ts_ms / 1000, 'unixepoch')` prints the local time the monitor
showed, and sorting on `ts_ms` is not time order across a clock change.
For real times use the time series, or subtract the monitor's UTC
offset for the stretch the row's `source_file` belongs to (a file under
`archive<N>/` was written before clock change N in
`airvisual_clock_changes`).

## Lightroom (Local)

`type = "lightroom"` — an Adobe Lightroom Classic catalog, the `.lrcat`
file (`catalog.path`). Mirrors every table of the catalog as a
deduplicated, versioned backup with full history; download-only,
nothing is rendered.

The catalog is wherever Lightroom keeps it — by default
`~/Pictures/Lightroom/Lightroom Catalog-v14.lrcat` or similar. It is
safe to run while Lightroom has the catalog open: a `VACUUM INTO`
snapshot is taken before reading. Only changed rows are stored on each
run, every prior state stays queryable through `dolt_history_<table>`
and `dolt_diff_<table>`, and an unchanged catalog produces no commit.
Query the raw store directly with `datalib-doltlite`.

`catalog.path` can also be one of Lightroom's backup `.zip` files. And
`backups.path`, beside the catalog or instead of it, is the folder
Lightroom writes its backups into (by default a `Backups` folder beside
the catalog): each backup becomes one commit, oldest first and dated
when it was taken, so the store holds the catalog's history from before
you started syncing it. Every later sync adds the backups taken since,
then mirrors the catalog on top.

### Limitations

- **The photo and video files are not copied, only the catalog.** Cause:
  Datalib's implementation. The source reads the `.lrcat` (and backup
  `.zip` files) and nothing else; the pictures the `photos` applet shows
  are read live from the `<catalog name> Previews.lrdata` folder beside
  the catalog, so they are missing when that folder is not reachable
  from this machine. `catalog_path` on the render step tells it where the
  catalog is now when Lightroom's own record of it is out of date.
- **A photo removed from the catalog leaves the grid at the next sync;
  only the store's history keeps it.** Cause: Datalib's implementation.
  Each sync empties the mirror and fills it again from the catalog, so
  the current tables, and the grid rendered from them, are the catalog
  as it is now. Every earlier version of each row stays in
  `dolt_history_<table>`. `when_deleted_upstream` is not offered.
  Nothing changes it.
- **History starts at the first sync, unless you point it at Lightroom's
  backups.** Cause: the file format: a catalog holds only its present
  state. `backups.path` (unset unless given) replays each backup in that
  folder as one commit, oldest first and dated when it was taken. A
  backup is only as detailed as Lightroom's backup schedule.
- **With a backups folder and no `catalog.path`, the copy is only as new
  as the newest backup.** Cause: the file format. If the newest backup
  the store holds is later removed from the folder, the copy stays on it
  and a problem says so until a newer backup arrives.
- **A photo whose camera recorded no time zone is shown as if taken in
  UTC.** Cause: the file format: most older cameras record none, and the
  offset Lightroom keeps lives only in each photo's XMP. Each such photo
  carries a warning. `capture_time_zone` on the render step (unset
  unless given) names the zone to use instead.
- **A capture time edited in Lightroom keeps the offset from before the
  edit, and a time with no seconds or a bare date gives the photo no
  capture time at all.** Cause: Datalib's implementation: the render
  reads `captureTime` and borrows the offset from the XMP's
  `DateTimeOriginal`, else `CreateDate`, and it parses only full
  date-times. Nothing changes it.
- **`skip_xmp` removes every photo's title and, for most photos, the time
  zone.** `skip_xmp` (off unless set) leaves out the per-image XMP packet
  and the search-index strings, the bulkiest columns. Cause: the file
  format (the XMP is the only place Lightroom keeps a title and the
  capture offset). The render then reports one problem on the source
  saying so.
- **Every change to the catalog renders every photo again.** Cause:
  Datalib's implementation: all photos are one render unit over 25 whole
  tables, so a sync that changed any of them redoes the whole grid.
  Nothing changes it.
- **A smart collection is not a collection on the photo.** Cause: the
  file format: a smart collection is stored as rules, with no member
  rows. Nothing changes it.
- **Tables Lightroom declares no key for diff by position.** Two stack
  tables (`AgLibraryCollectionStackData`, `AgLibraryFolderStackData` in
  the measured catalogs) have no key and no unique index, so a row added
  in the middle shows as many modified rows. Cause: the file format.
  `primary_keys` names a key for a table; a wrong one fails the whole
  sync.
- **If the snapshot of an open catalog fails, the copy may be torn, and
  only the log says so.** Datalib reads a `VACUUM INTO` copy; when that
  fails it copies the file with its `-wal`/`-shm`/`-journal` files, which
  can be caught mid-write while Lightroom runs. Cause: Datalib's
  implementation. Closing Lightroom before a sync avoids it.
- **An image with no file record in the catalog is left out of the grid
  without a problem row.** Cause: Datalib's implementation (the render
  joins each image to its `AgLibraryFile` row).
- **An uncollected store grows on every sync, a sync that changed
  nothing included.** `gc` (off unless set) runs `dolt_gc()` at the
  start of each sync; measured on ten real backups, collection removed
  about a third of the file. Cause: the storage engine.

### Reading the copy

Each photo or video is one document with a grid row of kind `Photo` or
`Video`; a virtual copy is a document of its own. A document has no
markdown body beyond a one-line summary; the `photos` applet draws it.
Its search terms are its keywords (`keyword:`), the people tagged on its
faces or by person keywords (`pictured:`, which `with:` reads too), its
rating (`rating:`) and each collection it is in (`collection:`). Nothing
is rendered as time series or occurrences.

The raw store, `<data_root>/<source id>/ingest/entities.doltlite_db`, is
the catalog table for table, under Lightroom's own names: `Adobe_images`
(one row per image), `AgLibraryFile`, `AgLibraryFolder` and
`AgLibraryRootFolder` (where its file is), `AgHarvestedExifMetadata`,
`AgLibraryKeyword` with `AgLibraryKeywordImage`, `AgLibraryCollection`
with `AgLibraryCollectionImage`, and `Adobe_AdditionalMetadata` (the
XMP).

- **A photo's identity** is `Adobe_images.id_global`. The other tables
  point at it by `id_local`, which Lightroom may renumber; join on
  `id_local` within one commit.
- **History** is the store's: `dolt_log()` lists the syncs and replayed
  backups that changed something, `dolt_history_Adobe_images` every
  version of a photo's row. A replayed backup's commit is dated when the
  backup was taken, so commit dates do not follow commit order.
- There is no "me": the catalog is one person's.

## Linear

`type = "linear"` — Linear's GraphQL API through latchkey (`api`).
Copies one workspace: issues with their history, comments, documents,
project updates and attachments, plus the teams, people, states,
labels, cycles, projects, milestones and initiatives they refer to.
Nothing is rendered; each kind of object is a table of the objects as
Linear sent them.

Sign in once in the browser:

```sh
latchkey auth browser linear
```

Or make a personal API key in Linear (**Settings** → **Security &
access** → **Personal API keys**; read access is enough) and store it
without `Bearer`:

```sh
latchkey auth set linear -H "Authorization: lin_api_…"
```

An empty `api` table copies the whole workspace. To copy part of it,
name teams by key or name in `teams` and projects by name in
`projects`. An issue is copied when its team or its project is named,
and most issues belong to no project, so name their teams. A name that
matches nothing selects nothing and shows as a problem on the source.
The first sync copies everything, newest first. Each sync after that
reads only what changed since, and for a quiet workspace that takes
seconds.

Archived objects are copied too, and nothing is deleted from the copy:
an object Linear deletes stays as it was last fetched. Files uploaded
into Linear (`uploads.linear.app` links) are not copied.

### Limitations

- **With nothing set, the whole workspace is copied, its whole history included.**
  An empty `api` table copies every team's issues, comments, documents,
  project updates and attachments, archived ones too. Linear has no
  `since` option: every record ever written is read on the first sync,
  however old. Cause: Datalib's own implementation (the config has no
  date bound). Nothing changes it, except naming `teams` or `projects`.
- **Naming `projects` alone leaves out most issues.** With `teams` or
  `projects` set, an issue is copied only when its team is in `teams`
  or its project is in `projects`, and most issues belong to no
  project. Cause: Datalib's own implementation, by design. Name the
  teams.
- **With a narrowed selection, some kinds of record are still copied
  for the whole workspace, and some are left out entirely.** Teams,
  people, workflow states, labels, project statuses and initiatives are
  always copied whole. Comments on documents and on initiatives, and
  documents that belong to an initiative or an issue rather than to a
  chosen team or project, are left out. Cycles are kept only for the
  chosen teams, so an issue brought in by a named project can point at
  a cycle the copy does not hold. Cause: Datalib's own implementation
  (the filters it sends Linear). Nothing changes it but copying the
  whole workspace.
- **Every attachment in the workspace is read even when the selection
  is narrowed.** Linear's API cannot filter attachments by their
  issue's team, so all of them are listed and the ones outside the
  selection are dropped after reading. Cause: the service's API
  (`attachments` takes no team filter). It costs requests on the first
  sync, and on later syncs only for attachments that changed.
- **Changing `teams` or `projects` reads every walked kind of record
  again from the newest.** What has been read is recorded per
  selection, so a new selection starts over. Records already held
  unchanged are not written again, so the cost is requests, not disk.
  Cause: Datalib's own implementation.
- **An edit is seen on the next sync, provided Linear moves the
  record's `updatedAt`.** Issues, comments, documents, project updates
  and attachments are read newest first by `updatedAt`, down to where
  the last sync reached, so an edit that moves `updatedAt` is picked up
  on the next sync. The small lists (teams, people, states, labels,
  project statuses, cycles, projects, milestones, initiatives) are read
  whole on every sync, so every change to them is seen. Cause: the
  service's API ordering. Nothing changes it.
- **A new relation between issues can be missed until the issue next
  changes.** Linear does not always move an issue's `updatedAt` when a
  relation is added, and relations are read only with their issue.
  Cause: the service's API. Nothing changes it.
- **A history entry Linear writes after the issue's own last update
  can be missed until the issue next changes.** On a real workspace, 2
  of 50 issues had such an entry, up to a minute later. Cause: the
  service's API (history is read again only when the issue's
  `updatedAt` moves). Nothing changes it.
- **Deletions are never noticed.** An archived record is still listed,
  with `archivedAt` set. A record Linear deletes outright stays in the
  copy as it was last fetched. A record that leaves a narrowed
  selection (an issue moved to a team that is not chosen) also stays,
  as it was before the move. `when_deleted_upstream` is not offered.
  Cause: Datalib's own implementation (nothing is ever removed).
- **Files uploaded into Linear are not copied.** Links to
  `uploads.linear.app` in descriptions and comments stay as links; the
  files need the credential to download, and nothing downloads them.
  Cause: Datalib's own implementation. Nothing changes it.
- **Some kinds of Linear object are not copied at all.** Only teams,
  people, workflow states, labels, project statuses, cycles, projects,
  project milestones, initiatives, issues, issue history, comments,
  documents, project updates and attachments are copied. Initiative
  updates, customers and customer requests, releases, notifications,
  favorites, views and templates are not (a comment's or a history
  entry's id for one of them can still appear). Cause: Datalib's own
  implementation.
- **Some fields are left out on purpose.** Anything that depends on who
  is asking (`isMe`, `favorite`, `subscribers`, anything `viewer…`),
  the AI summaries Linear regenerates on its own (`aiAnalysis`,
  `activitySummary`, `summary`), and the collaborative-editing state
  (`descriptionState`, `contentState`, `bodyData`), which holds the
  same text as the markdown field. One consequence: the copy does not
  record which person is the account it signed in as. Cause: Datalib's
  own implementation, by design.
- **A few lists inside a record are cut off without notice.** An
  issue's relations stop at 50, a project's teams at 50 and its members
  at 100, an initiative's projects at 100. Only the relations an issue
  holds itself are read (`relations`), not the ones other issues hold
  pointing at it (`inverseRelations`); to find what blocks an issue,
  look in the other issues' relations. Cause: Datalib's own
  implementation (the query asks for one page and no more). Nothing
  changes it.
- **An issue whose history did not finish fetching keeps that gap
  until the issue changes again.** An issue carries its first 50
  history entries; the rest are fetched separately. If that fetch
  fails, the issue is still stored as up to date, so later syncs do not
  try again until its `updatedAt` moves, and the problem row it left
  (`record:issues:<issue id>`) is never cleared, even after a later
  fetch succeeds. Cause: Datalib's own implementation.
- **A first sync of a large workspace takes minutes, not hours.**
  Measured: 10,551 issues, 12,247 comments and 56,161 history entries
  took 425 requests and about 6 minutes; a sync after it with nothing
  changed took 25 requests and 13 seconds. Linear allows 2,500
  requests an hour per user. A rate-limit refusal that names its wait
  is waited out, however long; one that names no wait ends the sync
  early, and the next sync carries on. Cause: the service's API.

### Reading the copy

Nothing is rendered: Linear has no documents, no grid rows of its own
and nothing in search. The grid shows only the source's storage report
(rows of kind `Source Size`, `Store` and `Table`), whose Items count is
the number of rows in `issues`. The copy is the raw store,
`<data_root>/<source id>/ingest/entities.doltlite_db`: one table per
kind of Linear object, each row `(id, payload)`, `payload` the object
as Linear's GraphQL API returned it. The main records are in `issues`,
`comments`, `issue_history` and `documents`.

Two things agents get wrong:

- **`updatedAt`, sort orders, progress series and counts are not in
  `payload`.** They are moved to `<table>_bookkeeping.volatile_payload`.
  For issues, comments, documents, project updates and attachments,
  `<table>_bookkeeping.held_version` is the record's `updatedAt`.
- **References are ids.** Another object is stored as `{ "id": … }`
  (`$.team.id`, `$.state.id`, `$.assignee.id`), or on comments and
  history as a plain id field (`$.issueId`, `$.toStateId`). Join to the
  other table's `id`. An issue's history is not inside the issue's
  payload: it is in `issue_history`, joined on `$.issue.id`. The store
  does not say which person is "me"; match `users` by the email the
  person signs in to Linear with.

## LinkedIn

`type = "linkedin"` — LinkedIn's "Get a copy of your data" export
(`export`). Mirrors messages, your posts and comments, the skills on
your profile, and connections as contacts. The rest of the export is
stored but not searchable, and a note on the source's Manage row
names those tables.

**Settings & Privacy** → **Data privacy** → **Get a copy of your
data**, request the full archive, and unzip it when the email arrives
(it can take a day):

```sh
unzip ~/Downloads/Complete_LinkedInDataExport_*.zip -d ~/backups/LinkedInDataExport
```

Point `export.path` at that directory, and unzip each newer export into
an empty one in its place: unzipped over the old one, a file the new
export dropped stays and is read again as current.

Connections come without photos: the export has none, and linkedin.com
shows a profile only to someone signed in.

### Limitations

- **The copy is only as new as the last export you unzipped.** Nothing
  is fetched from linkedin.com: every sync reads the folder of CSVs at
  `export.path` again, so a message, connection or post made after the
  export was requested is not in the copy until you request a new
  export and unzip it there. Cause: the export format (a one-time
  download LinkedIn emails you). Nothing in Datalib changes it.
- **There is no date limit.** The source has no `since` option; every
  row of every CSV in the export is stored, however old. Cause:
  Datalib's implementation (the config has only `export.path`).
- **Only messages, your posts and comments, your skills and your
  connections are searchable.** Every other CSV in the export (logins,
  invitations, reactions, job applications, ad-targeting data, search
  queries, endorsements, recommendations, positions, education and the
  rest) and your long-form articles are stored in the raw store but
  rendered by nothing, so they do not appear in the grid or in search.
  A note on the source's Manage row names the first five of those
  tables and counts their rows. Cause: Datalib's implementation; there
  is no option to turn more on. Read those tables from the raw store
  instead (see "Reading the copy").
- **A file left out of a newer export keeps its old rows.** When the
  newer export has no `Connections.csv` (LinkedIn lets you request a
  subset), the `connections` table keeps what the last export that had
  that file put there, and nothing marks it as old. The same happens
  if you unzip a newer export over the old folder: a file the new
  export no longer has is still on disk and is read again. Cause:
  Datalib's implementation, on purpose, so a partial export does not
  empty the copy. To drop a feed, unzip the new export into an empty
  folder that holds every file you want.
- **Something you deleted on LinkedIn disappears from the copy once a
  newer export without it is read.** Each CSV replaces its table
  whole: a connection you removed, or a message LinkedIn no longer
  exports, is deleted from the copy at the next sync after the new
  export is in place (earlier versions stay in the store's doltlite
  history). `when_deleted_upstream` is not offered. Cause: Datalib's
  implementation.
- **An edited record is a new record.** Most CSVs have no id column, so
  a row's id is a hash of its contents; when a row changes between
  exports (a message's folder, a post's text) the old row is deleted
  and a new one added, and its document id moves with it. Rows with a
  natural key keep their id: a connection is its profile URL, a share
  its `ShareLink`, a skill its name. Cause: the export format gives
  most records no id.
- **Two identical rows become one.** Two rows of a CSV with exactly the
  same values (for example two equal comments on one post whose `Date`
  is blank) share one id and are stored once. In the CSVs keyed on one
  column, rows that share that column are also stored once, the later
  row in the file winning: one job application per `Job Url`, one
  reaction and one instant repost per post `Link`, one followed company
  per `Organization`, one saved item per `savedItem`. Cause: Datalib's
  implementation of the row key.
- **Your own direct messages are not marked as yours.** In messages,
  every sender is named by their LinkedIn profile link, yours included,
  and the export does not say which profile is you, so `from:me` does
  not find your messages; search by your profile link or your name
  instead. Your posts and comments are marked as yours (by your primary
  email address). `to:me` finds no LinkedIn message: the export names
  recipients by name and profile link only. Cause: the export format.
- **Message attachments, subjects and folders are not shown.** A
  message's `ATTACHMENTS`, `SUBJECT` and `FOLDER` columns are kept in
  the raw store but not rendered, and no attachment file is fetched.
  Messages from every folder are rendered alike. Cause: Datalib's
  implementation.
- **A post you only commented on shows a placeholder.** Your comments
  are grouped by the post they were on; when that post is not one of
  yours, the export does not include it, so the thread opens with
  "Original post not included in the LinkedIn export" and a link to
  it. Cause: the export format.
- **Connections have no photo and no exact time.** The export has no
  photos, and "Connected On" is a date without a time, so the grid
  sorts a connection at midnight UTC of that day. A connection's email
  address is there only when that person chose to share it. Cause: the
  export format.
- **Times are read as UTC.** A message's `DATE` ends in `UTC`; a share's
  or comment's `Date` sometimes has no zone and is read as UTC too. A
  row whose date is in any other shape gets no time at all, and a
  problem is recorded on its document. Cause: the export format, read
  by Datalib's implementation.
- **A file that will not read keeps its old rows.** A CSV that is empty
  (0 bytes, or only LinkedIn's `Notes:` preamble with no header row),
  not UTF-8, or not well-formed CSV leaves its table as the last good
  read left it, and a `listing:csv <file>` problem stays until a sync
  reads it. A CSV with a header and no rows does empty its table.
  Cause: Datalib's implementation, on purpose.
- **Two CSVs that map to one table: only the last one is kept.** A
  table is named after the file's path with the trailing `_<digits>`
  LinkedIn adds to some names removed, so two files such as
  `Comments_111.csv` and `Comments_222.csv` (from two accounts' exports
  unzipped into one folder) both go to `comments`, and the second read
  replaces the first. Cause: Datalib's implementation. Keep one
  account's export per source.
- **Symbolic links inside the export are not followed.** A link is
  reported as a `listing:files` problem, and while any part of the
  folder cannot be read, no article is deleted from the copy. Cause:
  Datalib's implementation, for safety.

### Reading the copy

- **What is rendered**: one document per conversation in each
  message-shaped CSV (`messages.csv`, and LinkedIn's AI-assistant
  transcripts `guide_messages.csv`, `learning_coach_messages.csv`,
  `LearningCoachMessages.csv`, `learning_role_play_messages.csv`);
  one document per post of yours, holding the post and your comments on
  it (or per post you only commented on); one contact page per
  connection; and one "LinkedIn skills" page. Grid `kind` values:
  `LinkedIn Chat` and `LinkedIn Message`; `LinkedIn Post` and
  `LinkedIn Post Message`; `Contact` (channel `Connections`);
  `LinkedIn Skills` and `LinkedIn Skill`. No time series, no
  occurrences.
- **The raw store** is `<data_root>/<group id>/ingest/entities.doltlite_db`.
  It has no fixed tables: each CSV of the export becomes one table, named
  after its path (`messages.csv` → `messages`, `Email Addresses.csv` →
  `email_addresses`, `Jobs/Saved Jobs.csv` → `jobs_saved_jobs`), with an
  `id` and a `payload` holding the row as `{ "<CSV header>": "<cell>" }`.
  List them with `.tables`. Long-form articles are in `articles`, as raw
  HTML.
- **A conversation** is the rows of one table that share
  `CONVERSATION ID`; the same id in two different tables is two
  conversations.
- **Who is "me"**: your posts and comments are written as "Me" under the
  primary address in `email_addresses` (else the first listed, else your
  name from `profile`); that label is also the `account` of every row.
  In messages nothing marks you: find your messages by your own profile
  link in `SENDER PROFILE URL`.

## Markdown (Local)

`type = "markdown"` — a folder on disk (`fswalk.path`), such as an
Obsidian vault. Mirrors every `.md` and `.markdown` file under it, one
document per note, so notes show up in the grid and in search.

Nothing but a path. A note is its path under the folder: editing it
updates its document, and renaming or moving it replaces the document
with one at the new path. Search knows where a note is filed, the same
way it does for a PDF. Folders whose names start with a dot
(`.obsidian`, `.trash`, `.git`) are skipped. Each sync keeps the text
it read, so the copy holds earlier versions of a note even after the
file has changed or gone.

### Limitations

#### Limitations

- **Every `.md` and `.markdown` file under the folder is mirrored, at
  any depth, except in folders whose names start with a dot.**
  `.obsidian`, `.trash`, `.git` and every other dot-folder are never
  opened. A dot-*file* such as `.draft.md` outside them is read. Files
  matched by `ignore`, by a `.gitignore` inside a git repository, or by
  an `.ignore` file are skipped too. Cause: Datalib's implementation.
  `ignore` (empty unless set) adds patterns; nothing un-skips a
  dot-folder.
- **A link to a folder is not followed; a link to a file is read as
  that file.** Notes reached only through a symlinked folder are not in
  the copy. Cause: Datalib's implementation.
- **There is no `since`: every note is mirrored whatever its date.**
  Cause: Datalib's implementation.
- **A file over 16 MiB is not read, and keeps the text an earlier sync
  stored for it.** A note that grows past the limit stops updating,
  with a warning, and its document shows the old text. Cause: Datalib's
  implementation. `max_bytes` (16 MiB unless set) changes the limit.
- **An edit is seen at the next sync; a deleted file's note leaves the
  copy at the next sync.** The previous text stays in the store's
  history, but the current tables and the documents no longer show it.
  `when_deleted_upstream` is not offered. Cause: Datalib's
  implementation.
- **A rename or move is a new note.** The note is its path: moving a
  file gives it a new document id and drops the old one, so links to
  the old document break. Cause: Datalib's implementation.
- **Nothing is deleted after a scan that could not list a folder**, or
  while a changed file could not be read. A note deleted at the same
  time stays until a scan completes. Cause: Datalib's implementation
  (a vanished note may be where the unreadable one went).
- **Text that is not UTF-8 is stored with each bad byte replaced by
  U+FFFD**, and the note carries a problem saying from which byte.
  Cause: Datalib's implementation.
- **Links, embeds and tags are not followed or structured.** Obsidian's
  `[[Note]]`, `![[image]]` and `#tag` stay as text in the note; a note
  has no link to the notes it names. The YAML properties block at the
  top is shown as a code block and is not parsed into fields. Cause:
  Datalib's implementation.
- **Images, PDFs and other files beside the notes are not read.** Cause:
  Datalib's implementation. Point a `media` or `pdf` source at the same
  folder for those.
- **The date is the file's modification time, at whole seconds, in no
  named time zone.** There is no creation date: a file's mtime is not
  when the note was written, and a sync tool that rewrites files moves
  it. Cause: the file system.

### Reading the copy

- **One document per note**, with one grid row of kind `Note` and
  `source_label` `Markdown`. Its title is the file name without `.md`
  or `.markdown`; its front matter carries `source_path` (the path
  under the folder), `modified_at` and a `file://` `source_url`. The
  grid row's `modified_at` is the file's mtime; it has no
  `created_at`. No time series, no occurrences.
- **The raw store** is `<data_root>/<group>/ingest/entities.doltlite_db`.
  `markdown_files` holds each note's whole text under its path;
  `markdown_scan_meta` holds the folder the paths are under.
- **A note's path is its identity everywhere**: `markdown_files.id`,
  the grid row's `upstream_id`, and the `rel_path` of its
  `ingested_files` row. The rendered uuid is minted from the path alone,
  so an edit keeps it and a move changes it.
- A note's text in the store is the file's whole content, properties
  block included; the store's history holds every version a sync read.

## Media (Local)

`type = "media"` — a directory tree of music, photos and video
(`fswalk.path`). Mirrors every audio, image and video file with its
metadata — artist, album and track for music; camera, lens, exposure,
GPS and capture time for photos and video — plus `.m3u` playlists;
download-only, nothing is rendered.

Nothing but a path. Files are keyed on content hash, not path, so the
same song synced to three folders is one row with three locations, and
a second hash over just the signal (`payload_blake3`) survives
retagging. Query the raw store directly with `datalib-doltlite`:
`media_files` is one row per path, `media_items` one per content hash,
`media_visual` the camera, GPS and capture time, `media_playlists` and
`media_playlist_entries` the playlists.

### Limitations

- **Nothing in the library is searchable or shown in the grid.** `media` renders no documents; its only grid rows are the storage report Datalib writes about every source. Everything it learns is in the raw store, for SQL. Cause: Datalib's own implementation (no render side is built). Nothing changes it.
- **Only files with a known extension are looked at.** Audio: `mp3 flac wav wave aif aiff aifc m4a m4b aac alac ogg oga opus wma`. Images: `jpg jpeg jpe png gif tif tiff dng cr2 cr3 nef arw orf raf rw2 heic heif avif webp bmp`. Video: `mp4 m4v mov avi mkv webm wmv mpg mpeg m2ts mts 3gp`. Playlists: `m3u m3u8`. The match ignores case. Anything else (`.mp2`, `.ape`, `.jxl`, `.ts`) is not seen. Cause: Datalib's own implementation. Nothing changes it.
- **Many files are recorded with little or no metadata.** What a file is comes from its first bytes, so a file whose bytes match no format Datalib parses has `container = 'unknown'`. Tags and EXIF are read where the format allows; when a reader fails, the file is still recorded, without those fields and without any `problems` row. Cause: Datalib's own implementation (a metadata read failure is only logged at debug level). Nothing changes it.
- **Keywords, ratings, faces and other XMP or IPTC fields are not read.** Only EXIF fields (camera, lens, exposure, GPS, capture time, `ImageDescription`, `UserComment`) and audio tags (title, artist, album, album artist, composer, genre, date, track and disc numbers) are kept. Cause: Datalib's own implementation (no XMP reader). Nothing changes it.
- **A photo's capture time often has no time zone.** `captured_at` carries an offset only when the file wrote one (EXIF `OffsetTimeOriginal`, which most cameras omit); otherwise it is a local time with no offset. A video's time from QuickTime's `©day` tag keeps its offset; one taken from the movie header (`mvhd`) is written with `+00:00`, although many cameras store local time there. Cause: the file formats. Nothing changes it.
- **The `payload_blake3` "same recording" hash exists only for some formats.** It is computed for WAV, AVI, MP3, FLAC, JPEG, PNG, TIFF and DNG, MP4/M4A/MOV and HEIC; it is NULL for GIF, WebP, AIFF, Ogg, Matroska and WebM, for files Datalib could not parse, and for files over `payload_max_bytes` (8 GiB unless set). Re-encoding a file changes it even when nothing audible or visible changed. Cause: Datalib's own implementation. `payload_max_bytes` changes the size ceiling; nothing adds a format.
- **Files in iCloud Drive, Dropbox or OneDrive that are not on this computer are skipped.** A file with a size and no data on disk is treated as an online-only placeholder and not read, so it is not in the copy unless an earlier scan stored it; one warning in `problems` counts them. A filesystem that reports no data blocks for files it does hold (some network or compressed volumes) looks entirely online-only. Cause: Datalib's own implementation, a heuristic. `skip_dataless = false` (true unless set) reads them, which makes the sync client download each one.
- **There is no size limit by default.** Every file is read in full on the first scan and again whenever its size or times change, so a first scan of a large library takes as long as reading it from disk, twice for most files (once for the file hash and once for the payload hash). Cause: Datalib's own implementation. `max_bytes` (unset by default) skips larger files, with a warning naming each.
- **A file is examined once per distinct content.** Container, tags, EXIF and payload hash are read the first time a scan meets a file's bytes and never again, so a Datalib upgrade that reads more fields does not fill them in for files already stored. Cause: Datalib's own implementation. Only a change to the file's bytes, or a reset of the step, reads it again.
- **Copies are one item.** Files with identical bytes share one `media_items` row (and one `media_audio` or `media_visual` row) and have one `media_files` row each. A retagged copy is a different item; `payload_blake3` links the two where the format has a recipe. Cause: Datalib's own implementation.
- **A playlist entry with an absolute path or a URL never links to a file.** `resolved_path` is filled only for entries relative to the playlist's folder, and only when they stay inside the scanned folder; an absolute path (as iTunes and many players write) is kept as written with `resolved_path` NULL, even when it points into the scanned folder. Matching is by exact text, so a difference in letter case does not link. Cause: Datalib's own implementation. Nothing changes it.
- **A playlist's per-entry duration is kept only when it is a whole number of seconds.** `#EXTINF:9.009,` stores no duration. Cause: Datalib's own implementation.
- **HLS stream manifests are left out**, and so is any `.m3u8` containing an `#EXT-X-` line. A playlist over 16 MiB is not read (a warning). `playlists = false` (true unless set) leaves all playlists out and deletes the ones already stored at the next complete scan. Cause: Datalib's own implementation.
- **A deletion is noticed on the next complete scan and leaves the current copy.** A file gone from the folder loses its `media_files` row, and an item no path names is deleted with its audio and visual rows. There is no `when_deleted_upstream` option; deleted rows stay readable in the store's earlier commits. A scan that could not read part of the folder deletes nothing that run. Cause: Datalib's own implementation.
- **Hidden files and folders are scanned.** Names starting with a dot are walked, `.ignore` files are obeyed everywhere and `.gitignore` files only inside a git repository; links to files are read as those files, links to folders are not followed. Cause: Datalib's own implementation. `ignore` adds gitignore-style patterns (none by default).
- **A scan cannot be stopped part way, and a scan that is killed keeps none of its work.** Nothing is committed until the whole folder is done. The next scan does not read again the files the killed one hashed, but it parses again every item that was new. Cause: Datalib's own implementation.
- **`since` has no effect.** Every scan covers the whole folder. Cause: Datalib's own implementation.

### Reading the copy

`media` renders nothing: no documents, no grid rows of its own, no time series. Query the raw store, `<data_root>/<group id>/ingest/entities.doltlite_db`.

- `media_files` has one row per file, its `id` the path under the scan root and its `blake3` the item it holds; `media_items` has one row per distinct content, keyed by `blake3`. Join them on `blake3`. `media_audio` (tags) and `media_visual` (EXIF, dimensions, video codecs) hang off `media_items.blake3` and exist only when something was found, so use `LEFT JOIN`.
- A file whose item could not be read has no `media_files` row; it is in `problems`.
- `media_items.blake3` changes on any edit, a retag included; `payload_blake3` (with `payload_scheme`) is the hash that survives a metadata edit. Compare two `payload_blake3` values only when their `payload_scheme` is the same.
- Playlists: `media_playlists` per file, `media_playlist_entries` per line in order. Whether an entry's file exists is `LEFT JOIN media_files f ON f.id = e.resolved_path`, never a stored column.

## Notion

`type = "notion"` — web API through latchkey (`api`). Mirrors your
workspace's pages, with their comment threads and attachments; database
rows come along as pages.

Notion authenticates with a token from an integration you create at
[notion.so/profile/integrations](https://www.notion.so/profile/integrations).
Make it a **personal access token** if that choice is offered: it acts
as you and sees whatever you can see, so an empty `api` table mirrors
the whole workspace with no starting point. An internal integration
works too but starts with access to nothing — Notion scopes it per
page, so you would open each page you want mirrored, use the **⋯** menu
→ **Connections** → **Connect to**, and pick it (child pages inherit
the access). Either way, copy the secret, which starts `ntn_`.

In the app, choose **Add source** → **Notion**, name the account, open
**Paste a key**, paste the secret and press **Save sign-in**. The form
stores the token and the version header Notion needs together, then
checks the connection and names the person and workspace it reached.

From a terminal: every request also needs a `Notion-Version` header,
and the client deliberately sends neither the token nor the version —
latchkey injects both — so the stored credential must carry the version
too. Set both headers in one `auth set`:

```sh
latchkey auth set notion \
  -H "Authorization: Bearer $(pbpaste)" \
  -H "Notion-Version: 2026-03-11"
```

Omitting the version header gets every request rejected with
`400 missing_version`, and the run then fails loudly rather than
storing nothing. Verify the whole path — token and version — in one
call before running a sync:

```sh
latchkey curl "https://api.notion.com/v1/users/me"
```

A `200` with a JSON user body means you're set. `latchkey auth list`
reports this service's credential as `invalid` even when it works;
only a real request tells you.

To mirror only part of the workspace, list the pages in `api.roots`
(page ids or paste-able URLs); every page linked under them comes
along, and so does a database embedded in one of those pages, with its
rows.

### Limitations

- **By default the copy holds every page the credential can see, database rows included.** With a personal access token that is everything its creator can see in the workspace. With an internal integration it is only the pages someone connected to the integration, and what sits under them. Cause: the Notion API (`POST /v1/search` lists what the credential can see). `api.roots` (default empty, meaning the whole workspace) narrows it to what sits under the named pages and databases, at any depth, including databases inside those pages and their rows, plus the pages a root's own body links or mentions (one hop: not what sits under those, and not what they link).
- **A page that was edited before the last sync, and only became visible to the credential afterwards, is not picked up.** Each sync lists only pages edited at or after the newest edit the store has already looked at. A page shared with the credential late, or moved under a root without being edited, has an older edit time and is skipped. Cause: Datalib's implementation, built on the search's newest-edit-first order. `refresh_window_days` (unset means 0) lists that many days of older edits again on every sync.
- **An edit to a page is seen at the next sync.** A page is fetched again whenever its `last_edited_time` moves. Notion reports edit times to the minute, so every page edited in the same minute as the newest one already seen is listed again too. Cause: the Notion API. Nothing needs changing.
- **A database's new name reaches the copy only when the search lists that database again.** Page titles follow each edit; a database's title is read once and refreshed only when the search returns it. Cause: Datalib's implementation.
- **A page's comments are read again only when the page's own edit time moves.** Whether Notion moves a page's edit time for a new comment has not been measured, so a comment added to a page nobody edits may not reach the copy until the page is next edited. Cause: Datalib's implementation. Nothing in the config changes it.
- **A resolved comment thread disappears from the copy** the next time that page's comments are read. Notion's comment listing (`GET /v1/comments`) returns only unresolved comments, and Datalib deletes the stored comments a complete listing leaves out. Cause: the Notion API, together with Datalib's choice to follow the listing. Nothing changes it.
- **A page deleted in Notion stays in the copy.** A page moved to Notion's trash is marked: its document says "This page is in Notion's trash." A page deleted for good keeps that note; a page deleted without ever being seen in the trash, or no longer shared with the credential, stays as an ordinary page (measured: 9 of 52,726 pages in one store). A page that moves out from under the configured roots, or whose root is taken out of the config, also stays as it was. Cause: the Notion API (search never reports a deletion) and Datalib's implementation (it never deletes a page). `when_deleted_upstream` is not offered.
- **Only the current version of each page is copied.** There is no `since`: every page is copied whatever its age, but only as it is now. Earlier versions exist only as far back as the store's own history of what earlier syncs saw. Cause: the Notion API offers no page history.
- **A page's body is Notion's own markdown, not its blocks.** The block tree is not copied. Some blocks markdown cannot express are kept as links (bookmarks, embeds, tweets); `alias`, `drive` and `ai_block` blocks are lost, with a warning on the page; buttons and breadcrumbs are dropped without a warning. Cause: the Notion API's `GET /v1/pages/{id}/markdown`.
- **A very long page can be cut short.** When Notion leaves parts of a body out as too large, Datalib fetches each part separately, at most 64 per page per sync; past that the rest is missing and the page carries a warning. Cause: the Notion API truncates, and the cap is Datalib's (measured: one page wanted about 1,385 such fetches).
- **A database row that has a body shows only the body, not its properties.** The properties table is drawn only for a page whose body is empty. Of the properties, only title, text, URL, email, phone, number, checkbox, select, status, multi-select, people, date and the created/edited times are drawn; relations, formulas, rollups, files, unique ids and created-by/edited-by are not. All of them are in the raw store's payload. A database's schema (its property definitions and views) is not copied. Cause: Datalib's implementation.
- **Only files in a page's body are downloaded.** A page's cover and icon, and files in a database's files property, are not. A file over 1 GiB is skipped with a warning. `blob_size_limit_bytes` is not read by this source. `attachments` (default `true`) turns file downloads off. Cause: Datalib's implementation.
- **Most page authors are known only by an id.** A personal access token may read only its own user (`GET /v1/users/{id}` answers 403 for anyone else, and `GET /v1/users` is closed to it). A person is named when a people property or a comment names them; otherwise the copy holds the id alone. A user's name, once stored, is never read again, so a rename is not seen. Cause: the Notion API, and Datalib reading a user once.
- **The text a comment was made on is read once.** If that block is later edited, the thread still quotes the old text. Cause: Datalib's implementation.
- **Nothing in the copy says which Notion user the credential belongs to.** Cause: Datalib's implementation (it does not store `GET /v1/users/me`).
- **An internal integration without the "read comments" capability gets no comments.** The sync records one warning and goes on. `comments` (default `true`) turns comments off. Cause: the Notion API.
- **A first sync of a large workspace takes hours.** Notion allows about 3 requests a second (measured). After the listing, each page costs one request for its body and one for its comments, made one at a time. A later sync is usually two requests, one page of search results and the trash listing, plus two per edited page. Cause: the Notion API's rate limit; one request at a time is Datalib's.
- **In roots mode, a database under a root with 10,000 rows or more makes that sync list the whole workspace.** Notion caps a database query at 10,000 results, so the rows past it are found through the search instead, which costs a request per 100 pages of the workspace once. Cause: the Notion API.
- **`max_pages` (unset means no limit) stops the sync after that many pages.** The rest are listed by the next sync. It is a guard against a misconfigured source, not a way to mirror part of a workspace.
- **The table of contents is in A-to-Z order, not Notion's sidebar order.** Cause: Datalib's implementation (the store keeps no position among sibling pages).

### Reading the copy

- One rendered document per page: its title, the path of pages and databases it sits under, then its body; a page with no body (most database rows) shows its properties as a table instead. A page in Notion's trash says so under its title. Grid kind `Notion Page`.
- One rendered document per comment discussion, separate from its page: the quoted text it was made on, then each comment. Grid kind `Notion Comment Thread` for the thread and `Notion Comment` for each comment.
- One more document per source, "Notion TOC: Table-of-Contents (Datalib generated)", grid kind `Notion Table of Contents`: every page as a nested list. It is not in free-text search, and its row adds nothing to the Items count.
- No time series and no occurrences.
- The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`. `pages` holds the page objects (properties included), `page_markdown` the bodies, `comments` the comments, `tree` the title and parent of every page, database and block a page sits under.
- A database row is a `pages` row whose `parent_id` is the database's id; the database's title is its `tree` row (`kind = 'database'`). A page's title is not a column of `pages`; read it from `tree.title` or from the payload's `title`-typed property.
- A discussion is every `comments` row with the same `discussion_id`; `page_id` is the page it was collected for, and `parent_id` the block it hangs off when `parent_type = 'block_id'`.
- There is no "me": the store does not record which user the credential belongs to.

## PDFs (Local)

`type = "pdf"` — a directory tree on disk (`fswalk.path`). Mirrors
every PDF under it, converted to markdown and keyed on content hash, so
PDFs show up in the grid and in search.

Nothing but a path. Two copies of the same paper convert once and share
one row; renaming or moving a PDF preserves its identity. No OCR yet: a
scanned, image-only PDF is recorded (`needs_ocr = 1` in the source's
`pdf_documents` table) and produces no text.

### Limitations

- **A scanned PDF has no text in the copy.** There is no OCR. A page that is only an image is counted (`pdf_documents.ocr_page_count`) and shown in the rendered document as a one-line note; a document none of whose pages has text renders nothing and is not searchable. Cause: Datalib's own implementation (OCR is not built). Nothing changes it: `ocr = true` is refused when the config loads.
- **One page whose font cannot be decoded hides the whole document.** If any page uses a font with no mapping to Unicode (an Identity-H font without `ToUnicode`, or only Type3 fonts), the document is stored with `has_encoding_issues = 1` and renders nothing, so its readable pages are not searchable either. Cause: Datalib's own implementation, deliberate (garbled text would be indexed as if it meant something). Nothing changes it.
- **Text from a decodable font that still comes out garbled is not caught.** Only undecodable fonts are detected; wrong text from a font that decodes is indexed as it comes out. Cause: Datalib's own implementation.
- **Some layouts convert poorly.** Running heads and footers from a browser's "Print to PDF" mostly stay in the text, a floated box or caption (a Wikipedia infobox) can be mixed into the paragraph beside it, and a dense form (a tax form) comes out as a jumble of field labels. Cause: the PDF format (text is placed by position, with no reading order) and the converter Datalib uses. Nothing changes it.
- **Only files whose name ends in `.pdf` are read.** A PDF saved without the extension is not seen; the match ignores case (`.PDF` is read). Cause: Datalib's own implementation.
- **Files over 512 MiB are not read.** Such a file is a warning in `problems` and has no document; if an earlier scan read it before it grew, it keeps the row that scan wrote. Cause: Datalib's own implementation. `max_bytes` changes it (512 MiB unless set; `0` is refused).
- **A PDF that would expand past fixed limits when parsed is refused.** A file is measured before it is parsed, and one with a single stream over 1 GiB decoded, over 64 GiB of streams in total, or over 64 million compressed objects is not read; it is an error row in `problems`, measured again every scan. Cause: Datalib's own implementation, to keep a crafted PDF from exhausting memory. Nothing changes the limits. Some crafted PDFs still get past the check (a chain of cross-reference streams, an encrypted document's object streams); one that makes the step run out of memory fails the whole step, and no `problems` row says which file.
- **A damaged or unreadable PDF is left out, not half-stored.** A file the classifier cannot read is an error row in `problems` with what the parser said, has no `pdf_paths` row and no document, and is tried again on every scan. Cause: Datalib's own implementation.
- **What a document is said to be is decided once.** A document is classified (scanned or text, page counts, title, author, dates) the first time a scan meets its bytes, and never again, even after a Datalib upgrade that classifies better. Cause: Datalib's own implementation. Only a change to the file's bytes, or a reset of the step, classifies it again. The rendered text is not affected: a new render version re-converts every document.
- **Copies of one PDF are one document.** Files with identical bytes share one `pdf_documents` row and one rendered document, whose shown location is the first of its paths in sort order. A PDF whose metadata alone was changed (retitled, or its `/ID` regenerated) is a different document; `content_blake3` links the two. Cause: Datalib's own implementation (a document is named by the hash of its bytes).
- **The author and title are what the file says, which is often wrong.** `author` is frequently a producer's username or an internal code rather than a person; `title` falls back to a title the converter guessed from the first page, and the grid shows the file name when the title looks like a path. A multi-author list shows as `First Author et al.` in the grid. Cause: the PDF format (producers fill these fields as they like). Nothing changes it.
- **A document's date is the date written inside the PDF, never the file's date.** The grid row's time is the PDF's `CreationDate`, else its `ModDate`; a PDF with neither has no date. A date given only as a year reads as 1 January of that year at midnight; one with no time zone is stored with none. Cause: the PDF format and Datalib's own implementation. Nothing changes it.
- **The PDF's bytes are not kept in Datalib.** The raw store records what each document is; the render step reads the file from disk to convert it. If a file changes between the sync and the render, its document shows a note saying its pages are not shown until the next sync, with a warning in `problems`. Cause: Datalib's own implementation.
- **A deletion is noticed on the next complete scan and the document leaves the current copy.** A file gone from the tree loses its `pdf_paths` row, and a document no path names is deleted with its rendered page. There is no `when_deleted_upstream` option; the rows stay readable in earlier commits of the store. A scan that could not read part of the tree deletes nothing that run. Cause: Datalib's own implementation.
- **Hidden files and folders are scanned.** Names starting with a dot are walked (a `.Trash` folder included), `.ignore` files are obeyed everywhere and `.gitignore` files only inside a git repository. Links to files are read as those files; links to folders are not followed. Cause: Datalib's own implementation. `ignore` adds gitignore-style patterns (none by default).
- **A PDF stored only in iCloud Drive is downloaded when it is read.** On a Mac with "Optimize Mac Storage", reading an evicted file makes macOS fetch it, so a first scan of such a folder downloads every PDF in it. Cause: the operating system; Datalib's PDF scan does not check for evicted files. An `ignore` pattern is the only way to leave a folder out.
- **A first scan reads every PDF in full; later scans read only new or changed files.** A file whose size, modified time, change time, inode and device match the last scan is not opened again. Cause: Datalib's own implementation.
- **A scan cannot be stopped part way, and a scan that is killed keeps nothing.** Nothing is committed until the whole tree is done, so a killed scan leaves the copy as the previous scan left it. Cause: Datalib's own implementation.
- **`since` has no effect.** Every scan covers the whole tree. Cause: Datalib's own implementation.
- **Rendered rows hold this computer's absolute path.** A grid row's `source_url` is a `file://` URL, so moving the folder changes every row, and two computers indexing the same files produce different rows. Cause: Datalib's own implementation, deliberate (it is what lets the app reveal the file).

### Reading the copy

One rendered document per distinct PDF (by its bytes), titled with the PDF's own title, else its file name. The grid has one row of kind `PDF Document` for it and one row of kind `PDF Page` for each page that has text; a page without text is a note in the document and has no row. A document all of whose pages lack text, or with an undecodable font (`has_encoding_issues = 1`), is not rendered at all. No time series, no occurrences.

The raw store is `<data_root>/<group id>/ingest/entities.doltlite_db`. `pdf_documents` has one row per distinct PDF, keyed by `blake3` (the lowercase hex hash of the file's bytes); `pdf_paths` has one row per file, its `id` the path under the scan root and its `blake3` the document it holds. Join them on `blake3`. The absolute path of a file is `pdf_scan_meta.abs_root || '/' || pdf_paths.id`. A PDF that would not classify is in neither table; it is a row in `problems`.

`needs_ocr = 1` means at least one page has no text, not that the document was skipped; whether it renders is `has_encoding_issues = 0 AND page_count > ocr_page_count`.

## Perseus Digital Library

`type = "perseus"` — a public download (`github`). Mirrors TEI
editions of Greek and Latin texts from the Perseus Digital Library.

No credentials. An empty `github` table pulls 13 editions and
translations of Thucydides' *Histories* from PerseusDL's
`canonical-greekLit`; `files` picks others, though only Thucydides is
rendered. Since
it needs nothing of yours, it is a good first sync to check an install
with.

Aligning two editions sentence by sentence uses a pinned
Ancient-Greek-BERT model, about 450 MB, fetched once on first use into
`~/.cache/datalib/models` (or `$XDG_CACHE_HOME/datalib/models`) and
checked against its pinned sha256 on every load.

### Limitations

#### Limitations

- **Only Thucydides' *Histories* is rendered.** By default the download
  fetches 13 editions and translations of it (Greek, English, French,
  German, Italian, Latin) and its `__cts__.xml` catalog file. A file
  named in `files` that is not an edition of that work (any name not
  starting `tlg0003.tlg001.`) is downloaded but never becomes a
  document, and nothing says so. Cause: Datalib's implementation (the
  work is fixed in the code). Nothing in the config changes it.
- **Only text inside book / chapter / section divisions is read.** Text
  outside a `<div subtype="section">` (or a chapter with no sections)
  is dropped, as are divisions whose numbers are not whole numbers,
  such as front and back matter. Notes and other markup inside a
  section are read as part of its text, with no structure. Cause:
  Datalib's implementation.
- **Every sync downloads every file again, in full.** The download
  makes no conditional request, so an unchanged file costs the same as
  a new one. Cause: Datalib's implementation.
- **The files come from the `master` branch as it is at sync time, and
  only the newest copy is kept.** There is no store and no history: a
  sync overwrites each file, so an upstream correction replaces the
  earlier text with no record of what changed. Cause: Datalib's
  implementation (Perseus keeps a plain file tree, not a store).
- **A file taken out of `files` is not removed.** It stays in the
  source's folder and is still rendered. Cause: Datalib's
  implementation. Delete the file from `<data_root>/<group>/ingest/` by
  hand.
- **Two files with the same name from different folders overwrite each
  other**, since every file lands in one folder under its base name.
  Cause: Datalib's implementation.
- **A file that fails to download fails the whole step.** The other
  files are still fetched, and the earlier copy of the failed one is
  kept, but the step ends as failed rather than with a problem row.
  Cause: Datalib's implementation (there is no store to hold a problem
  row).
- **The dates on Perseus rows are not real dates.** Each book and
  chapter is given a made-up time in 2026, counting up in reading
  order, so that sorting the grid by date reads the work from the
  start. Cause: Datalib's implementation.
- **Editions are not linked to each other unless asked for.** With
  `alignment_pairs` unset, a chapter in one edition has no edge to the
  same chapter in another; the shared locator (book.chapter.section) in
  each row's title is the only connection. Setting it downloads a pinned Ancient-Greek-BERT model, about
  450 MB, on first use, and makes rendering much slower. Cause:
  Datalib's implementation. `alignment_pairs` (empty unless set) on the
  render step turns it on.
- **The download needs `curl` on the `PATH`.** Cause: Datalib's
  implementation.

### Reading the copy

- **Documents**: one per book (`kind` `Book`, a page with no text that
  the chapters sit under in the source's tree) and one per chapter per edition (`kind`
  `Chapter (<edition id>)`, such as `Chapter (perseus-grc2)`). Each
  section of a chapter is a grid row of its own, `kind`
  `Section (<edition id>)`. Every row has `source_label` `Perseus` and
  `project` `Thucydides, Histories`. No time series, no occurrences.
- **A chapter or section row links to the same passage on
  scaife.perseus.org** (`source_url`).
- **Editions are linked to one another only for the pairs in
  `alignment_pairs`**: one edge from a chapter to the same chapter in
  each edition it is paired with, and one per aligned pair of
  sentences. With it unset there are no edges.
- **There is no raw store.** The source's folder,
  `<data_root>/<group>/ingest/`, holds the TEI XML files as downloaded
  (`tlg0003.tlg001.<edition id>.xml`, `__cts__.xml`), and the render
  reads them directly. Read the documents and grid rows, or the XML.
- The edition id's last part names the language: `perseus-grc2` is
  Greek (`grc`), `1st1K-eng1` English. Edition titles come from
  `__cts__.xml`.
- `created_at` on these rows is a made-up reading-order stamp, not a
  date.

## Signal backups

`type = "signal"` — a folder of Android backups (`backup.path`). Mirrors
messages and media. Every backup in the folder is kept as a version of
the store, dated when it was taken, so two days' backups can be compared
for messages that went missing or changed.

Signal stores encrypted backups on the phone. Enable backups in the app
(Settings → Chats → Backups), then pull the backup directory off the
device over `adb`:

```sh
adb pull /sdcard/Signal/SignalBackups ~/backups/SignalBackups
```

You'll also need the 64-character recovery key shown when you enabled
backups.
The provider reads it from the environment variable `aep_env_var` names;
the app writes `SIGNAL_BACKUP_PASSPHRASE` there. Name the variable even
when it is that one: the runner hands a step only the variables its
params name.

### Limitations

- **Only Signal Android's newer local backups are read: a folder of
  `signal-backup-YYYY-MM-DD-HH-MM-SS/` snapshots, unlocked with the
  64-character recovery key.** The classic single `.backup` file with a
  30-digit passphrase is not read, and neither is Signal Desktop or
  Signal on iPhone. Cause: Datalib's implementation reads only that one
  backup format. Nothing in the config changes it.
- **The copy holds the whole account as of the newest backup in the
  folder, and nothing newer.** Datalib never talks to Signal; it reads
  the files you copied off the phone. A message sent after the newest
  backup you copied is not in the copy until you copy a newer backup
  into the folder and sync. Cause: the export format (a local backup on
  the phone). Nothing in the config changes it.
- **There is no `since`: every message the backup holds is read.**
  Cause: Datalib's implementation; the `backup` table takes only `path`
  and `aep_env_var`, and refuses any other key. Nothing can narrow it.
- **The first sync decrypts every backup in the folder, one after
  another.** Each snapshot is read whole and kept as its own version of
  the store, dated when Signal took it. A folder of 30 daily backups
  costs 30 full decrypts on the first sync. Later syncs read only the
  backups the store does not yet hold. Cause: Datalib's implementation
  (it keeps every backup as a version so two can be compared). To make
  the first sync shorter, leave only the backups you want in the folder.
- **An edit is seen at the next backup, as a changed message.** The
  stored message is the one the backup holds, so an edit on the phone
  shows up when a newer backup is read. The rendered page shows only
  the current text. The earlier versions Signal keeps with an edited
  message are stored in the message's `payload` but never shown. Cause:
  Datalib's implementation (the render does not read them). Nothing in
  the config changes it.
- **A deletion is followed at the next backup, and the copy then drops
  the message.** A message no longer in the newest backup is deleted
  from the current copy (it stays in the store's history, one version
  per backup). `when_deleted_upstream` is not offered. A message
  "deleted for everyone" stays, but as a deleted message with no text,
  and it is not shown. Cause: the export format (each backup is the
  whole account) and Datalib's implementation. Nothing in the config
  changes it.
- **When the newest backup the store has read is removed from the
  folder, the copy stays on it.** Datalib will not move back to an older
  backup, because that would look like losing every message only the
  newer backup had. The Manage row shows an error until a newer backup
  arrives. Cause: Datalib's implementation. Copy a newer backup in.
- **Only messages with text or an attachment are shown.** Reactions,
  stickers, calls, group and chat changes ("X joined", timer changes),
  shared contacts, payments, polls, view-once messages, story replies
  and gift badges are stored and counted, but no page shows them and
  search does not find them. One warning on the Manage row lists how
  many of each the last backup held. Cause: Datalib's implementation.
  Nothing in the config changes it.
- **A quoted reply links to the message it quotes, but the quoted text
  is not shown.** When two messages in a chat were sent in the same
  millisecond, the reply is not linked at all. Cause: Datalib's
  implementation (a quote names its target only by send time).
- **Only the attachments of an ordinary message are saved.** The rest
  of a long message (Signal keeps text past its first part in a separate
  file), sticker images, link preview images and quote thumbnails stay
  in the backup and are never decrypted, so a long message shows only
  its first part. Sticker packs, call links, notification profiles and
  chat folders are not stored at all. Cause: Datalib's implementation.
  Nothing in the config changes it.
- **An attachment the phone never downloaded has no bytes.** Signal
  leaves its key out of the backup, and the Manage row shows a warning
  for it. If an earlier backup in the folder had the bytes (for example
  from an old phone, before a switch), the copy keeps them. Cause: the
  export format.
- **Attachments must be in the `files` folder beside the snapshots.**
  There is no option for a `files` tree kept somewhere else. Cause:
  Datalib's implementation (the download has a setting for it that the
  config does not offer).
- **A backup that will not open is skipped, not fatal.** One that is
  half copied (a file missing) or corrupt shows as an error on the
  Manage row and is tried again on the next sync; the others are read.
  If no backup the run tried opens, most likely because the recovery
  key belongs to another account, the whole sync fails. Cause: Datalib's
  implementation.
- **A backup with a part that would not decode deletes nothing.** The
  messages in that part are missing from the copy, and messages the
  backup seems to have dropped are kept, until a newer backup reads
  cleanly. The Manage row shows an error (`phase:frames`). Cause:
  Datalib's implementation, chosen so a bad read never deletes.
- **A stored message that the render cannot decode disappears from the
  page without a warning.** Cause: Datalib's implementation, an
  accidental limitation (the render treats an unreadable stored message
  as one with no text and drops it).
- **A stop is honoured only between backups.** A backup that has begun
  is read to its end. Cause: Datalib's implementation.
- **Times are to the millisecond with no time zone.** A backup records
  when a message was sent as an instant, so a rendered time ends in `Z`
  (time zone unknown). Cause: the export format.

### Reading the copy

- **One rendered document per chat**: a one-to-one chat, a group, or
  Note to Self, with every message in it. Grid rows have kind
  `Signal Chat` (the document) and `Signal Message` (one per message
  shown). No time series and no other row kinds.
- The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`;
  attachment bytes are in `blobs.sqlite` beside it. Messages are in
  `chat_items`, chats in `chats`, people and groups in `recipients`.
  `docs/user/raw_stores.md` § "Signal (`signal`)" lists every column.
- **"Me"** is the recipient whose id is `self`. A message you sent has
  `author_id = 'self'` in `chat_items`, and the page shows it as "Me".
- **A chat's id is the id of who it is with**: `chats.id` equals
  `chats.recipient_id` (`aci:…`, `e164:+…`, `group:…`, `self`), except
  for the rare chat that has nothing stable to go by.
- **A message's id is `<chat>#<author>#<date_sent>`**, and the grid row's
  `upstream_id` is that same string. `date_sent` is milliseconds since
  1970.
- **Each backup is a version of the store.** `dolt_log()` has one commit
  per backup, `download signal: backup <folder name>`, dated when Signal
  took it. To see what changed between two backups, diff the two
  commits (`docs/dev/doltlite.md` § "What the upstream deleted on you").

## Slack

`type = "slack"` — web API through latchkey (`api`). Mirrors the
channels the account is in and their file attachments; DMs and the
channels it has not joined when the config asks for them.

Built into latchkey — no manual export:

```sh
latchkey auth browser slack
```

`since` in the `api` table is the oldest message to mirror: a day
(`"2025-01-01"`) or an offset from the sync (`"-30d"`); unset is the
whole history, and a first sync of a big workspace then takes hours.
Messages older than a sliding `since` stay mirrored. The
user list and bookmarks are re-read at most every `list_every_hours`
(6 unless set).

Archived channels are left out unless the `api` table sets
`archived = true`, or names the channel in `channels`. Set it together
with `all_channels = true`: Slack drops almost everyone from a channel
it archives, so without `all_channels` it adds almost nothing. Each
archived channel is read once; later syncs don't ask it for anything
new unless Slack unarchives it.

### Limitations

- **With nothing set, the copy holds only the channels the account is a
  member of, public and private, and no direct messages and no archived
  channels.** Cause: Datalib's own defaults. `dms = true` adds 1:1 and
  group DMs (`dm_conversations` narrows them); `all_channels = true` adds
  every channel the account can see but has not joined; `archived = true`
  adds archived channels (meant together with `all_channels`, since Slack
  drops most members from a channel it archives); a list in `channels`
  takes exactly the channels it names, joined or not, archived or not.
  With `dms` off, Datalib never asks Slack for DMs, so none are in the
  store at all.
- **Every channel added with `all_channels` or `channels` that the
  account has not joined costs a request on every sync, even when
  nothing was posted.** Cause: Datalib's implementation. A sync learns
  which conversations have news from one `client.counts` call, which
  names only the conversations in the account's sidebar; a channel it
  does not name is asked through `conversations.history`. One measured
  workspace took 80 seconds for 126 quiet channels asked this way. The
  same happens for every conversation when the token may not call
  `client.counts`. Nothing changes it but mirroring fewer channels, or
  `scope = "involves_me"`.
- **The refresh window re-reads the last 30 days of every mirrored
  conversation once an hour, so a wide scope costs requests every hour.**
  Cause: Datalib's implementation; Slack has no "what changed" call.
  `refresh_window_days` (30 unless set; `0` turns it off) and
  `refresh_every_minutes` (60 unless set) set the size and how often.
- **An edit or a new reaction on a reply in a thread is not seen until
  someone posts a new reply in that thread.** Cause: the API, plus
  Datalib's implementation. `conversations.history` lists only a
  thread's root, and Datalib reads a thread's replies again
  (`conversations.replies`) only when the root's `latest_reply` changes;
  an edit or a reaction does not change it, and the search for new
  replies looks only for replies newer than the one already held. This
  holds however recent the reply is, and the refresh window does not
  help. A reply that was also sent to the channel is the exception: it
  is listed in the channel's history, so its edits are seen inside the
  refresh window. Nothing else changes it.
- **An edit or a new reaction on a top-level message is seen only if the
  message was posted within the last 30 days, and up to an hour late.**
  Cause: Datalib's implementation (the refresh window). The window is
  counted back from the sync's own time by when the message was posted,
  not when it was edited. Older top-level messages are read again only
  when their thread gets a new reply: then the root is read again with
  it. A top-level message with no replies, older than the window, is
  never read again. `refresh_window_days` (30) and
  `refresh_every_minutes` (60) change it.
- **A DM closed in the account's Slack sidebar is checked once a day,
  so its new messages, edits and deletions can be up to a day late.**
  Cause: `client.counts` names only conversations open in the sidebar,
  and Slack's search cannot prove a DM empty. A closed DM is walked on
  the first sync that finds it closed and then once a day; between
  those checks it is not read at all, refresh window included. Nothing
  changes it.
- **An archived channel is read once and never again while it stays
  archived.** Cause: Datalib's implementation (archived channels take no
  new messages, and `client.counts` rarely lists them). If Slack
  unarchives it, the next sync reads it as usual.
- **A deleted top-level message is dropped from the copy only if it was
  posted within the last 30 days; an older deletion is never noticed.**
  Cause: the API. Slack keeps no record of deletions; a message just
  stops being listed, so Datalib can only see a deletion by re-reading
  the stretch (the refresh window). A root that goes takes its stored
  replies with it. `refresh_window_days` (30) changes how far back.
  `when_deleted_upstream` is not offered: a deletion that is seen is
  always copied (the store's history keeps the old row).
- **A deleted reply is usually not noticed until someone posts in that
  thread.** Cause: as for reply edits above. Deleting a reply leaves
  the root's `latest_reply` where it was, unless the deleted reply was
  the newest. When it was the newest and the root is inside the refresh
  window, the next re-read of the window notices it.
- **A new reply to an old thread arrives at least 30 minutes after it
  was posted.** Cause: Datalib's implementation. A root older than the
  refresh window is never listed again, so new replies to it are found
  with Slack's `search.messages`, which Datalib asks only for replies
  posted more than 30 minutes before the sync (Slack indexes a reply a
  little after it is posted). A token that may not search gets a
  `listing:search.messages` problem, and then new replies to roots
  older than the refresh window are not found at all.
- **Nothing older than `since` is read.** Cause: Datalib's
  implementation. Unset, `since` is the whole history, and a first sync
  of a big workspace then takes hours. Moving `since` earlier later reads
  the gap once; moving it later deletes nothing.
- **Some content is not captured.** Slack Lists (the session token is
  refused `slackLists.*`); files hosted outside Slack (for example a
  linked Google Drive file) and deleted files, which carry no bytes to
  fetch; file bytes when `media = false`, or over
  `common.blob_size_limit_bytes` (no limit unless set); a workspace's
  custom emoji pictures, which stay `:name:`; link-preview pictures.
  Profiles and the bot list change in the copy at most every
  `list_every_hours` (6 unless set); a bot's name is asked once and not
  again.
- **With `scope = "involves_me"`, a channel where nothing mentions the
  account, is posted by it, or says `@here` / `@channel` is not read at
  all, and a read channel starts only `context_before` (1 day) before
  the oldest such message.** Cause: Datalib's implementation, through
  Slack search. Measured on one account over 14 days: 103 of 104
  involving messages were found; the one missed was the account's own
  "joined the channel" notice, which search does not list. Mentions of
  a user group the account is in are not searched for. DMs are read as
  with the default scope.
- **How far the account has read a followed thread is only as fresh as
  that thread's last reply.** Cause: the read mark comes on the thread
  read, which happens only when a new reply arrives. A conversation's
  own read mark comes from `client.counts` on every sync.

### Reading the copy

One rendered document is one thread: a top-level message and all its
replies; a message no one replied to is a thread of one. Its grid rows
are one `Slack Thread` row per document, one `Slack Message` row per
message and one `Slack Reaction` row per reaction; the workspace, and the
channel or DM, are places the rows sit in (`Slack Workspace`,
`Slack Channel`, `Slack DM`). No time series. The raw store is
`<data_root>/<id>/ingest/entities.doltlite_db`: messages in `messages`,
conversations in `channels`, people in `users`, bots in `bots`.

- **Me** is `workspaces.self_user_id`; a message's author is
  `messages.user_id`. A bot posts with a `bot_id` in the payload and
  often no `user_id`: its name is the payload's `username`, else
  `bot_profile.name`, else `bots.name`.
- **A thread** is `WHERE thread_root_uuid = <root id>`, root included.
  `is_thread_root = 1` marks every top-level message, replied to or
  not. A reply that was also sent to the channel is stored once, as a
  reply.
- **A thread may be incomplete**: compare the root payload's
  `reply_count` with the replies stored under it; the document says
  "N not mirrored yet" while they differ.

## SMS Backup and Restore

`type = "sms_backup_restore"` — the app's XML export on disk
(`backup.path`). Mirrors Android SMS, MMS and call logs, one chat per
number.

Android texts and call logs come from the free **SMS Backup & Restore**
app by SyncTech: <https://www.synctech.com.au/sms-backup-restore/>
(also on the
[Play Store](https://play.google.com/store/apps/details?id=com.riteshsahu.SMSBackupRestore)).
It exports messages and calls as XML — `sms-<timestamp>.xml` (SMS + MMS)
and `calls-<timestamp>.xml`.

In the app, tap **Set up a backup**, select **Messages** and **Call
logs**, and back up to **local storage** (not just the cloud). Leave
**Include MMS attachments / media** enabled — the app inlines photos,
audio recordings, etc. as base64 directly in the XML, which is how we
pick them up as attachments.

Then pull the XML files off the device into one directory:

```sh
adb pull /sdcard/SMSBackupRestore ~/backups/SMSBackupRestore
```

(Or copy them over MTP / a file manager / the app's share sheet.) Point
the source's `backup.path` at that directory — it walks every `*.xml`
inside, so keeping multiple dated backups there is fine; re-ingesting a
newer export deduplicates against what's already there, except a
message whose type changed between backups (queued, then sent), which
is stored twice.

### Limitations

- **The copy holds only what the backup files in the folder hold, and is as fresh as the newest file you copied over.** Datalib reads `.xml` files on disk; it does not talk to the phone or to the app, and cannot ask for a new backup. Cause: the export format (the source is a folder of files). What changes it: running a new backup in the app and copying it into `backup.path`.
- **Every message and call in every backup file is mirrored; there is no way to mirror less.** The source has no `since` and no filter by contact or date; its only option is `backup.path`, and the config refuses any other key. Cause: Datalib's implementation. Nothing in the config changes it; to mirror less, keep fewer files in the folder.
- **A message you delete on the phone stays in the copy as long as any backup file in the folder still holds it.** Datalib deletes a message or call only when no file in the folder holds it any more, and only on a run that read every file without an error. Backups are snapshots, so an older backup still holds what you deleted later. Cause: the export format. What changes it: removing the older backup files from the folder. `when_deleted_upstream` is not offered.
- **If any backup file fails to read, nothing is deleted on that run.** A file that cannot be opened, does not parse, is cut off before its closing tag, or is empty holds deletions back until it reads cleanly; the store records this as a problem. Cause: Datalib's implementation, on purpose (an unreadable file may hold any message). What changes it: fixing or removing the bad file.
- **A backup file that does not parse adds none of its messages.** A file cut off part way (a copy that stopped early) is refused whole, not read up to where it stops. Cause: Datalib's implementation. What changes it: copying the file again in full.
- **A draft, a queued message, or a message that failed to send is shown as one you received.** Only Android's "sent" type (2) counts as yours; every other type, outbox, draft, failed and queued included, is filed as incoming from the other number. Cause: Datalib's implementation (an accidental limitation). Nothing can change it today.
- **The same message can appear twice if two backups record it with different types**, for example once as queued and once as sent. A text's identity is its number, time, type and body, so a change of type makes a second record, and both stay while both files are in the folder. Cause: Datalib's implementation. What changes it: removing the older file.
- **The same person can be split across two conversations.** A conversation is keyed on the number as the backup wrote it, reduced to `+` and digits, so `+12025550101` and `2025550101` are two conversations. Cause: Datalib's implementation. Nothing can change it today.
- **A number written without its country code is not linked to a person.** Only a number that starts with `+` (and, under `+1`, has ten digits after it) becomes a phone handle; a number without one still shows its digits but is not found by `with:` and is not matched to a contact. Short codes (`8880`) and named senders (`VERIZON`) are not handles either. Cause: Datalib's implementation. Nothing can change it today.
- **A group MMS is its own conversation, keyed on all its members' digits run together in the order the phone listed them.** A text one member sent you alone is in a different conversation. If the phone lists the members in another order, that is another conversation. Cause: the export format (a group's address is the members' numbers joined by `~`) and Datalib's implementation. Nothing can change it today.
- **MMS pictures, audio and video are only there if the backup was taken with media.** A part with no data is not stored; the store records one warning per file saying how many parts were empty. A part whose base64 does not decode is an error on that attachment. Cause: the export format (the app inlines media only when "Include MMS attachments" is on). What changes it: a new backup with media turned on.
- **An MMS's layout (SMIL) is dropped; its text parts are joined into one body.** Cause: Datalib's implementation. Nothing can change it.
- **Reactions, read receipts and delivery reports are not captured as such.** Android's text-message "reactions" arrive as ordinary text messages and are shown that way. Cause: the export format holds no reaction records. Nothing can change it.
- **Contact names are the ones the phone had when the backup was taken.** The app writes a `contact_name` on each record; Datalib keeps it and does not look the number up anywhere else. Cause: the export format. What changes it: a newer backup.
- **A message's time of day in the phone's own time zone is not kept as a time.** The backup gives an instant in milliseconds, which Datalib stores in UTC; the phone's zone survives only as the app's `readable_date` text. Cause: the export format. Nothing can change it.
- **Whether a message was read comes from the backup, and is shown only for a message someone else sent that the backup marks unread.** A backup that says nothing leaves no mark. Cause: the export format. What changes it: a newer backup.
- **Nothing is readable until a whole run finishes.** A run reads every changed file, holds all of it in memory, media included, and writes once at the end; there are no checkpoints part way. A first run over a large folder of backups with media can need a lot of memory. Cause: Datalib's implementation. Nothing can change it today.

### Reading the copy

One rendered document per conversation: every text, MMS and call with one number (or one group), its whole history in one document, oldest first. Calls appear in the conversation as notes ("Missed call", "Outgoing call · 1:35"). The grid rows are kind `SMS Conversation` for the document and `SMS Message` for each text, MMS and call; the Source column says `SMS`. It renders no time series and no occurrences.

The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`. Texts and MMS are in `sms_messages`, calls in `sms_calls`, both with a `conversation_key` that is the number reduced to `+` and digits; MMS media bytes are in `blobs.sqlite`, joined through `sms_attachments`.

- **Telling "me" from others:** a message you sent has `box_kind = 'sent'` (and `is_me` true in its payload); everything else is `'inbox'`. In the rendered document your messages are by "Me". An MMS's sender is the `addrs` entry with `type` 137.
- **Assembling a conversation:** take `sms_messages` and `sms_calls` rows with the same `conversation_key` and order them by `when_ts`. A grid row's `upstream_id` is the `sms_messages.id` or `sms_calls.id` it came from.

## Todoist

`type = "todoist"` — Todoist's API through latchkey (`api`). Copies
your projects, sections, labels, open tasks and their comments, and the
tasks you have completed. Each project becomes one document: its
comments, then each section's open tasks with their subtasks and
comments, then the section's completed tasks, folded.

Sign in once in the browser; latchkey reads your API token from
Todoist's settings for you:

```sh
latchkey auth browser todoist
```

Or copy the API token yourself (**Settings** → **Integrations** →
**Developer**) to your clipboard and store it:

```sh
latchkey auth set todoist -H "Authorization: Bearer $(pbpaste)"
```

The token can change your tasks as well as read them; Todoist has no
read-only token. The download only reads.

An empty `api` table copies everything. Completed tasks are read back
to the day you joined Todoist, a quarter at a time; set `since` (a
day, an RFC 3339 instant, or an offset from the sync such as `"-90d"`)
to stop sooner, or `completed = false` to leave them out. Each sync
after the first reads only what changed; one with nothing new is a
single request.

Nothing is deleted from the copy: a task, project or comment you delete
is kept, marked deleted, and left out of the documents. Not copied: the
files attached to comments (their links are kept), comments on tasks
completed before the first sync, and filters, reminders and the
activity log. An archived project's own row may be missing; its
completed tasks still come, in a document titled by the project's id.

### Limitations

- **With no `api` table in the step's params, nothing is downloaded.**
  An empty `[steps.params.api]` copies everything below; leaving the
  table out plans no download at all. Cause: Datalib's own
  implementation (the download runs only when `api` is present).
  Changes it: add `[steps.params.api]`.
- **By default the copy holds every active project, section, open task,
  comment and personal label, and every task completed since the day
  the account joined Todoist.** This includes shared projects and the
  people who share them (collaborators). Not asked for at all: filters,
  reminders, view options, workspaces and the activity log. Cause:
  Datalib's own implementation (the `resource_types` it asks `/sync`
  for). Nothing in the config changes it.
- **`since` limits only the completed tasks.** Every open task, project,
  section and comment comes in the first sync whatever `since` says,
  because `/sync` answers with all active data and has no date filter.
  `since` (a day, an RFC 3339 instant, or an offset such as `"-90d"`)
  only stops the read of completed tasks; without it that read goes
  back to the account's `joined_at`. Cause: the service's API (`POST
  /sync` has no time window). `completed = false` (default `true`)
  turns the completed-task read off; tasks already stored stay.
- **Comments on a task that was completed before the first sync are not
  copied.** `/sync` sends comments on active tasks only, and the
  completed-tasks endpoint carries only a count (`note_count`).
  Comments on a task completed after the first sync are kept, because
  they arrived while the task was open. Cause: the service's API, plus
  Datalib's own implementation (it does not make the
  `GET /comments?task_id=` call per task that would fetch them).
  Nothing in the config changes it.
- **An archived project may be missing, and its open tasks with it.**
  A full sync sends only active data, and `GET /projects/archived` is
  not read. Its completed tasks still come, and render puts them in a
  document titled `Project <id>` that says the project was not sent.
  Cause: the service's API, plus Datalib's own implementation (the
  archived-projects endpoint is not called). Nothing in the config
  changes it.
- **Files attached to comments are not downloaded.** The comment's
  `file_attachment` is kept as Todoist sent it, URL included, and the
  document links to that URL. Cause: Datalib's own implementation.
  Nothing changes it.
- **Shared labels appear only as names on the tasks that carry them.**
  The `labels` table holds personal labels only. Cause: the service's
  API. Nothing changes it.
- **Reactions on comments are stored but not shown.** They are in the
  comment's payload (`reactions`); the document and grid leave them
  out. Cause: Datalib's own implementation.
- **Edits to open tasks, projects, sections and comments are seen on
  the next sync, with no time limit.** `/sync` sends everything that
  changed since the last token. A task re-opened after completion
  shows as open again.
- **A completed task's row from the completed-tasks list is read once
  and not read again.** Each stretch of completion time is read once
  and then marked as read; later syncs read only the time since the
  last one. A change made later to a task completed before the first
  sync reaches the copy only if `/sync` reports it. Whether `/sync`
  reports an edit or a deletion of such a task has not been checked
  against a real account. Cause: the service's API (the completed list
  has no "changed since" form) and Datalib's own implementation.
  Nothing in the config changes it.
- **Nothing is ever removed from the copy.** A project, section, task,
  comment or label deleted in Todoist is kept with `is_deleted: true`,
  as `/sync` sent it, and left out of the documents. A deleted project's
  document is removed. `when_deleted_upstream` is not offered. Cause:
  the service's API reports deletions as records, and Datalib keeps
  them.
- **A person who neither is the account nor shares a project now is
  shown as "Todoist user <id>".** Names and emails come only from the
  account's own record and from `collaborators`. Cause: the service's
  API. Nothing changes it.
- **Whether each completion of a recurring task is listed has not been
  checked.** Completing a recurring task moves its due date and leaves
  it open, so the open task shows only its next due date. Cause: the
  service's API.
- **If `/sync` fails, the whole run stores nothing.** Any answer other
  than success fails the run and keeps the old sync token, so the next
  run asks again from the same point. What Todoist answers for a sync
  token it no longer accepts has not been seen; if it answers an error,
  every run fails the same way. Cause: Datalib's own implementation.
- **If the account's record has no `joined_at` and `since` is not set,
  completed tasks are not read**, and a problem row says so. Cause:
  Datalib's own implementation (it needs a start date). Set `since` to
  read them.
- **A first sync of an old account makes many requests.** The
  completed-tasks endpoint answers for at most three months at a time,
  so the read is cut into twelve-week windows, newest first, with up to
  200 tasks a page: an account ten years old costs about 44 requests
  before counting extra pages. Todoist allows 100 full syncs and 1000
  incremental syncs per user per 15 minutes; a rate-limited request
  waits as long as Todoist asks, up to 10 minutes when the wait is
  named in the error body. Cause: the service's API. A run with nothing
  new is one request.

### Reading the copy

Each Todoist **project** is one document: its comments, then each
section in Todoist's order, each section's open tasks as a tree
(subtasks under their parent), each task's comments after it, then the
section's completed tasks, newest first, folded under "✓ N completed".
Tasks in no section, or in a deleted section, come first, under no
heading. A subtask whose parent is completed, deleted or in another
project is shown at the top level. Grid rows have the kinds
`Todoist Project` (the document), `Todoist Task` and `Todoist Comment`;
a task or comment row's `channel` is its section's name. Nothing is
rendered as a time series.

The raw store is `<data_root>/<source id>/ingest/entities.doltlite_db`.
Open tasks are in `items`; completed tasks are in `completed_items`,
and a task completed after the first sync is in both. When a task is
in both, the `items` row is the current one. "Me" is the one row of
`user`: its `id` is the user id that tasks and comments name in
`added_by_uid`, `responsible_uid` and `posted_uid`. A task names its
labels by name, not by `labels.id`.

## WhatsApp backups

`type = "whatsapp"` — an Android `crypt15` backup (`backup.path`).
Mirrors messages and media.

Only the end-to-end-encrypted `crypt15` format with a 64-digit key is
read; a backup protected by a password cannot be decrypted offline.

**Get the key.** No root is needed. In WhatsApp, go to Settings → Chats
→ Chat backup → End-to-end encrypted backup, turn it on and choose the
**64-digit key** option, not a password. Write that key down. If you
already turned it on with a password, turn it off and on again with the
64-digit key. The sync reads the key from the environment variable
`key_env_var` names; the app writes `WHATSAPP_BACKUP_DECRYPTION_KEY`
there. Name the variable even when it is that one: the runner hands a
step only the variables its params name.

**Pull the backup.** Trigger a fresh local backup first (Settings →
Chats → Chat backup → Back Up) so it is current, then plug the phone in
with USB debugging on and pull WhatsApp's whole folder off over `adb`
(or copy it through MTP or a file manager):

```sh
adb pull /sdcard/Android/media/com.whatsapp/WhatsApp/ ~/backups/WhatsApp/
```

Point `backup.path` at that folder. It holds
`Databases/msgstore.db.crypt15`, the messages, which is required;
`Backups/wa.db.crypt15`, the names your phone shows for each contact
(without it chats are named by phone number); and `Media/`, the
attachments (without it they show as placeholders, and copying it in
later fills them in). Profile photos are in no backup.

The whole decrypted database is kept — every table, versioned across
backups, the way the Lightroom and Apple Photos sources keep theirs —
so a message you delete on the phone is still in an earlier commit of
the store. Three tables WhatsApp rewrites on every backup (`props`,
`backup_changes`, `frequent`) are left out by default (`skip_churn`),
so a backup with no new messages makes no new commit.

### Limitations

- **The copy is only as new as the backup folder you last copied off
  the phone.** Datalib reads a folder on this computer; it never
  reaches the phone or WhatsApp's servers. A message sent after the
  last backup you pulled is not in the copy. Cause: Datalib's
  implementation (the one ingest method, `backup`, reads local files).
  What changes it: make a new backup on the phone (Settings → Chats →
  Chat backup → Back Up), pull the folder again, and sync.
- **Only Android backups in the `crypt15` format with the 64-digit key
  can be read.** An iPhone backup, an older `crypt14`/`crypt12` file, or
  a backup protected by a password is refused. Cause: the export
  format; a password-protected backup's key is held by WhatsApp's
  servers, and Datalib reads only the 64-digit-key form. Nothing in
  the config changes it.
- **By default every table of the message database is copied, except
  three that WhatsApp rewrites on every backup.** `props`,
  `backup_changes` and `frequent` are left out (`skip_churn`, `true`
  unless set). Everything else in `msgstore.db` is copied
  (`include_tables`, `["*"]` unless set; `exclude_tables` and
  `exclude_columns`, empty unless set). Cause: Datalib's choice, so
  that a backup with no new messages makes no new version of the
  store.
- **A message deleted on the phone disappears from the copy at the
  next sync, and a chat deleted on the phone loses its document.** Each
  sync replaces every table with what the newest backup holds. The
  earlier version of the store still holds the deleted rows, and they
  can be read from there, but they are no longer rendered or searched.
  The same applies when a phone is set up again without restoring its
  history: the next sync empties the chats it no longer has. Cause:
  Datalib's implementation (the mirror drops and refills every table).
  `when_deleted_upstream` is not offered for this source, and nothing
  else keeps a deleted message in the rendered copy.
- **Changes between two backups are seen; anything that happened and
  was undone between them is not.** Each sync records the state of
  one backup. Cause: the export format (a backup is a snapshot).
- **There is no `since`: the whole backup is read on every sync.**
  Whatever history the phone holds is copied, however old. Cause:
  Datalib's implementation (the mirror has no date filter). Nothing
  changes it.
- **Only the plain text of a message is shown.** The document shows
  `message.text_data` alone. A media caption kept in
  `message_media.media_caption`, and the link preview kept in
  `message_text`, are copied into the store but not rendered. Where a
  backup keeps a message's body only in those tables, the message
  shows with no text. Cause: Datalib's implementation (an accidental
  limitation of the render). Nothing changes it; the raw store holds
  the full rows.
- **A reply is not linked to the message it quotes, and @-mentions are
  not recognised.** The quoted message is copied into the store but not
  drawn. Cause: Datalib's implementation (the render sets no reply or
  mention). Nothing changes it.
- **Calls, group membership, polls, locations and contact cards are
  copied but not rendered as such.** Only messages, their attachments
  and their reactions become documents. A message of a kind the render
  does not know (anything below type 50 without an attachment) shows
  as a plain text message, often empty; type 50 and above show as a
  system line. Cause: Datalib's implementation. Nothing changes it; the
  tables are in the raw store.
- **Times are shown in UTC with the local clock marked as unknown.**
  WhatsApp stores milliseconds since 1970 with no time zone, so a
  rendered time ends in `Z`. Cause: the export format.
- **Without `Backups/wa.db.crypt15`, chats are named by phone number or
  by a raw `…@lid` id.** The names your phone shows live in that second
  file. A backup copied without it keeps whatever names an earlier sync
  stored. Cause: the export format (names are not in the message
  database). What changes it: copy the `Backups/` folder too.
- **Profile photos are never copied.** They are in no backup;
  `Media/WhatsApp Profile Photos` holds only photos someone saved by
  hand. Cause: the export format. Nothing changes it.
- **Without the `Media/` folder, every attachment shows as a
  placeholder.** Copying `Media/` in later and syncing again fills them
  in. Cause: the export format (media sit beside the database, not in
  it).
- **Of two media files with identical bytes, only one is linked to its
  message; the other message shows a placeholder and a "not among the
  media files the backup holds" problem.** This happens when WhatsApp
  keeps two copies of one picture, for example a forwarded photo.
  Cause: Datalib's implementation (an accidental limitation: the media
  registry keeps one row per distinct content, so the second path is
  not recorded). Nothing in the config changes it. The bytes are in
  the store under the first path.
- **A whole sync stops if the message database will not decrypt.** A
  wrong key, a truncated file or an unknown format fails the sync, and
  the copy stays as the last good sync left it. Cause: Datalib's
  implementation. A `wa.db` or a `Media/` file that will not read costs
  only itself and is recorded as a problem.
- **A large backup is read whole into memory on every sync.** The
  message database is decrypted in memory before it is written to a
  temporary file, so memory use grows with the size of
  `msgstore.db`. Media are hashed only when their size or time changed
  since the last sync. Cause: Datalib's implementation.

### Reading the copy

One rendered document is one chat, with every message in it, whatever
its age. Its grid rows carry the kinds `WhatsApp Chat` (the chat),
`WhatsApp Message` and `WhatsApp Reaction`. No time series and no
occurrences.

The raw store, `<data_root>/<group>/ingest/entities.doltlite_db`, holds
WhatsApp's own message database (`msgstore.db`) table for table, under
WhatsApp's table names (`jid`, `chat`, `message`, `message_media`, …),
plus two tables of Datalib's: `wa_db_contacts` (the names from
`wa.db`) and `wa_media_files` (the attachments found in `Media/`).

- **Me**: a `message` row with `from_me = 1` is one you sent. The
  backup does not say your own number.
- **Who sent it**: in a group, `message.sender_jid_row_id` points at
  `jid._id`; in a one-to-one chat it is empty, and the sender is the
  chat's own `jid`.
- **Every reference is a row number**: `message.chat_row_id → chat._id`,
  `chat.jid_row_id → jid._id`. A chat's lasting identity is
  `jid.raw_string`; a message's is that plus `message.key_id` and
  `message.from_me`.

## YoLink

`type = "yolink"` — YoLink's API (`api`). Mirrors per-device sensor
history — temperature, humidity, water — rendered as one page of
charts.

Each device is one `devices` entry naming its `kind`
(`temperature_humidity` or `watermeter`), the `start` date to mirror
from, and two 32-hex-character ids, `family_device_id` and
`device_udid`. The ids are effectively read secrets that cannot be
rotated, and they go straight into the config — keep that file to
yourself. `all_sources.toml` shows the shape.

### Limitations

- **Only the devices listed in the config are mirrored, and only
  temperature-and-humidity sensors and water meters.** Cause: Datalib's
  implementation; there is no device discovery, and the history
  download is read for two device kinds only. Changes it: add a
  `devices` entry per device; another kind of device cannot be added.
- **Nothing older than about 66 days before a sync can be fetched,
  whatever `start` says.** Cause: YoLink's history download serves only
  a trailing window, and answers an older request with an empty,
  successful response (measured on one account at about 66 days).
  Datalib does not ask for anything older than 66 days. Nothing changes
  it: history is kept only if Datalib synced while YoLink still served
  it.
- **A stretch with no sync for more than about 66 days is lost for
  good.** Cause: YoLink's retention, as above. Changes it: sync at
  least every few weeks; the mirror keeps everything it already holds.
- **A reading that reaches YoLink more than 5 minutes after it was
  taken, for a stretch Datalib has already fetched, is never fetched.**
  Cause: Datalib's implementation: each stretch is fetched once, and
  only the 5 minutes before it are asked for again. Changes it:
  `overlap_minutes` (5 unless set).
- **A sensor that has stopped reporting looks like a quiet sensor.**
  Cause: YoLink answers an empty body for a stretch with no readings,
  whether the device was quiet or dead. Datalib adds a warning when a
  device's newest reading is more than a day old (`silent:<device>` in
  `problems`). Nothing else changes it.
- **Temperatures are fetched in °C and water in US gallons; any other
  unit makes that stretch fail to read.** Cause: Datalib asks YoLink for
  those units and refuses a reply in another. Nothing changes it.
- **A reading row with one value missing keeps the other value; a row
  that cannot be read makes the whole stretch (`window_days`, 7 days)
  fail and be asked for again.** Cause: Datalib's implementation.
  Changes it: nothing, except that a smaller `window_days` makes the
  stretch smaller.
- **Renaming a device in the config starts a new history; the old
  readings stay under the old name.** Cause: Datalib's implementation:
  the config `name` keys every reading. Nothing changes it.
- **A device taken out of the config is still shown, with its old
  readings, but is no longer fetched or checked for silence.** Cause:
  Datalib's implementation: device rows are never removed. Nothing
  changes it.
- **Edits and deletions do not apply.** A reading is never changed or
  removed once stored; `when_deleted_upstream` is not offered.
- **Only YoLink's US server (`us.yosmart.com`) is asked.** Cause:
  Datalib's implementation. Whether devices on other YoLink regions can
  be mirrored has not been established.
- **A device's two ids are effectively a password that cannot be
  changed, and `family_device_id` is also kept in the raw store.**
  Cause: YoLink's signed history download needs only those two ids.
  Anyone holding the config, or `family_device_id` with `device_udid`,
  can read that device's history. Nothing changes it; keep the config
  and the raw store private.

### Reading the copy

The render writes one page for the whole source: a chart per quantity
(temperature, relative humidity, liquid volume, with the water meter's
lifetime total on a second chart), and a section per device. Grid
`kind` values: `Sensor Timeseries` (the page) and `Sensor Device` (one
row per device, its name in `channel`). Plots convert to SI (°C,
litres); the raw store and the time series keep the unit YoLink sent.

The `render_time_series` step writes one time series per device and
metric, named `<device>/<metric>`, by instant (Unix milliseconds), into
`<data_root>/<group>/render_time_series/time_series.doltlite_db`. Its
`time_series` table can be read with SQL; the points are compressed in
`time_series_chunks` and are read through the app,
`POST /applet/unified_time_series_index/points` (the app must be
running). For plain numbers, the raw store is simpler: every reading is
one row with its value in a column.

The raw store is `<data_root>/<group>/ingest/entities.doltlite_db`.
Readings are in `yolink_readings`, one row per device, time and metric;
`device_name` is the device's `name` in the config, the same as
`yolink_devices.id`. The water meter writes two metrics per reading:
`water_meter_gal` is the meter's lifetime total, which only climbs, and
`water_consumption_gal` is what was used since the previous reading.
Sum the second, never the first, to get consumption over a period.
