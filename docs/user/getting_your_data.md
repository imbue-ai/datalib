# Getting your data

One section per source, in alphabetical order, saying what the source
mirrors and how to get a copy of your data where the pipeline can read
it: either credentials stored in `latchkey`, or an export on disk. This
doc is only about *access* — for the config file and running the sync,
see the [first-time user guide](first_time_user.md), and for one
fully-commented config entry per source see
[`all_sources.toml`](config_examples/all_sources.toml).

Each section opens with the source's `type` — the word that goes in the
config and picks the code that runs — and where the data comes from.
*Where* is one table on the source's ingest step, named under the type:
`api` is the product's own API, `export` an unpacked export, `backup` a
phone backup, `fswalk` a folder to scan. A type that is not one product
spells its sources out — email's are `jmap`, `gmail` and `mbox`. A table
that reads files carries its own `path`.

Conventions: exports land under `~/backups/`, and `latchkey` is the
one the datalib installer put on your `PATH` (it runs on the Node
runtime bundled in the same tarball). Adjust paths to taste and
point the matching source in your config at them. Wherever a command
takes a secret, it is written as `$(pbpaste)`: copy the secret to your
clipboard, then run the command. Your shell history keeps the harmless
`$(pbpaste)` text rather than the secret itself.

The credentials `latchkey` stores are the same session your browser
has. Anything that can run commands as you can use them to act as you
on that service — see the warning in the first-time guide before
storing any.

## AirVisual

`type = "airvisual"` — reads each monitor's own history files
(`export`). Mirrors particulates, AQI, CO₂, temperature, humidity and
VOC as time series, rendered as one page of interactive plots.

An AirVisual Pro (IQAir's indoor air-quality monitor) keeps its history
on the unit — no cloud account is involved — and serves it over a Samba
share: `smb://<ip>/airvisual`,
user `airvisual`, with the password shown on the device under
**Settings › Network › Access Pro data**. Mount the share (in the
Finder, **Go › Connect to Server…**) and point one `devices` entry at
the mount, or at a copy of that folder:

```toml
[[steps.params.export.devices]]
path = "/Volumes/airvisual"
```

Every `YYYYMM_AirVisual_values.txt` under the path is read, the
`archive*/` folders the device starts on each clock change too; a
later run re-reads only the month being written. Each device's
identity is its serial and its name is what it calls itself, both read
from the folder's `latest_config_measurements.json`; set `serial` for a
copy without it, and `name` to override what the device says.

## Apple Messages

`type = "apple_messages"` — reads the Messages app's own `chat.db` on a
Mac, or a copy of it (`database.path`). Mirrors iMessage and SMS chats
with tapbacks; attachments are listed by name, their bytes are not
copied.

The file is `~/Library/Messages/chat.db`. An iPhone backup's
`3d0d7e5fb2ce288813306e4d4636395e047a3d28` is the same database and
works too. macOS protects `~/Library/Messages`: in the app, choose the
file with the picker — that is what grants Datalib access (Cmd-Shift-G
in the dialog reaches the folder). From a terminal, the terminal needs
Full Disk Access (System Settings → Privacy & Security); an ingest that
reports "Operation not permitted" is missing that, not the file.

The whole database is mirrored table for table with its history, so a
message deleted on the Mac is still in an earlier commit of the store.
Every chat renders as a page a month.

## Apple Photos

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
`<library>/originals/<X>/<UUID>.<ext>`; a [media](#media) source pointed
at that folder gives you their EXIF and content hashes.

## Beeper

`type = "beeper"` — reads the Beeper Texts desktop app's local data
directory (`texts.path`). Mirrors the chats of whichever networks you
pick — Signal, Google Chat and the rest Beeper bridges.

On macOS the directory is `~/Library/Application Support/BeeperTexts`;
no credentials. Lightly used — expect rough edges. It never notices a
deletion, and `always_clear_before_ingest` is the wrong fix here (the
provider's `INGEST.md` says why).

## ChatGPT

`type = "chatgpt"` — web API through latchkey (`api`). Mirrors your
conversations.

A one-time registration, then a browser login. ChatGPT uses a bearer
access token rather than a cookie, and latchkey can go and fetch it
for you. The app's Add Data Source wizard does both from its
**Latchkey auth** button (and **Test connection** then lists the
account's conversations to pick from); by hand it is:

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
about ten days and `services info chatgpt` cannot tell when it has
gone (it reports `unknown`, not `invalid`); when a sync or **Test
connection** comes back `HTTP 401 token_expired`, run that same
`auth browser` line again.

If it still says `token_expired` right after a fresh login, look for
a *second* service on the same address: `latchkey curl` picks the
first registered service whose base URL matches, so an older
registration for `https://chatgpt.com/` — a test one, say — is the
credential every request actually carries, whatever you just stored
under `chatgpt`. `latchkey services list` shows them; `latchkey
services deregister <name>` removes the stale one.

If you registered `chatgpt` before this guide said to, latchkey will
have recorded it as a `set`-only service — `latchkey services info
chatgpt` shows `authOptions` without `browser`, and a name that
already exists cannot be re-registered. Run `latchkey services
deregister chatgpt` first, then the two commands above.

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

## Claude

`type = "claude"` — web API through latchkey (`api`), **or** an
unpacked Claude data export on disk (`export`). Mirrors conversations
across every org, and projects.

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

The downloader fetches through `latchkey curl` and clears Cloudflare's
managed challenge with the bundled Chrome-impersonating curl, so no
`cf_clearance` cookie is needed — the `sessionKey` cookie is the entire
auth surface.

If you would rather not store a live session at all, request a data
export from Claude's settings and give the `claude` source an `export`
table pointing at the unpacked folder instead. The export is a complete
snapshot, so a re-exported tree that no longer mentions a conversation
drops it from the store.

## Claude Code

`type = "claude_code"` — reads the transcripts Claude Code keeps on
this machine (`sessions`). Mirrors every session run in the terminal,
the desktop app or an IDE extension, one document per session and one
per subagent, with tool calls and results folded away.

No credentials and nothing to export: an empty `sessions` table reads
the standard store, `~/.claude/projects`. Give it a `path` for a copy
of that folder from another machine. A session Claude Code later
deletes stays in the mirror.

## Contacts

`type = "contacts"` — a CardDAV server through latchkey (`carddav`),
**or** local `.vcf` files (`vcf`). Mirrors your address book.

- **A `.vcf` export.** Most address books export vCards; point
  `vcf.path` at a directory of them. No credentials. The directory is
  the whole address book, so the sample config sets
  `always_clear_before_ingest = true` to let a missing `.vcf` mean a
  missing contact.
- **A CardDAV server.** Credentials go in latchkey under a service
  whose base URL matches the server. Fastmail's is built in and takes
  an app password (Settings → Privacy & Security → Integrations → App
  passwords, with contacts access):

  ```sh
  latchkey auth set fastmail-dav -u "you@fastmail.com:$(pbpaste)"
  ```

## Email

`type = "email"` — a Google Takeout `.mbox` on disk (`mbox`), **or** a
JMAP server other than Fastmail through latchkey (`jmap`). Mirrors mail
messages and their attachments. [Gmail](#gmail) and
[Fastmail](#fastmail) have sections of their own; this one is the
other two routes into the same source.

**A Takeout `.mbox`.** Google Takeout's **Mail** product is a single
`.mbox` file; see [Google Takeout](#google-takeout) for requesting and
unpacking one. Point an `mbox` table's `path` at it. No credentials.

**Another JMAP server.** Any RFC 8620/8621 server works the way
Fastmail does: register its host as a latchkey service, store the
credential it takes, and use a `jmap` table with its `hostname`.
Fastmail is the one that is built in.

## Facebook

`type = "facebook"` — Facebook's "Download your information" export,
in JSON (`export`). Mirrors posts, photo albums, comments and
reactions; friends as contacts; every other file of the export goes to
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

## Fastmail

`type = "email"` — Fastmail's JMAP API through latchkey (`jmap`).
Mirrors mail messages and their attachments.

Fastmail is built into latchkey, including the region-prefixed API
hosts (`phl.api.fastmail.com` and the like) that an account homed in a
regional datacenter gets. The browser flow stores an OAuth token:

```sh
latchkey auth browser fastmail
```

If you would rather use an API token, create one at
[app.fastmail.com/settings/security](https://app.fastmail.com/settings/security)
under **Integrations** → **API tokens** → **New API token**, give it
read access to your mail, copy it, and store it instead:

```sh
latchkey auth set fastmail -H "Authorization: Bearer $(pbpaste)"
```

Use a `jmap` table with `hostname = "api.fastmail.com"`. Fastmail's
contacts are a separate route — see [Contacts](#contacts).

## Garmin

`type = "garmin"` — Garmin Connect's API, with its own login rather
than latchkey (`api`). Mirrors per-day health metrics (sleep, heart
rate, stress, body battery, HRV, SpO₂, …), weigh-ins, activities with
their original FIT files, devices, records, gear, badges, workouts and
goals; the weigh-ins render as one page with an interactive plot.

Garmin's API wants a bearer minted by a signed request that latchkey
cannot make, so the provider signs in on its own. Run
`datalib-step login garmin` once — it asks for your Garmin email,
password and the MFA code Garmin emails you, and writes a token that
lasts about a year under `~/.garth` (a token from the `garth` Python
tool works too). Then add the source from the wizard or from the
`all_sources.toml` example; `since` says how far back to mirror. The
first sync makes one request per metric per day since `since`, so a
long history takes a while; later syncs re-read only the trailing week.

## GitHub

`type = "github"` — web API through latchkey (`api`). Mirrors pull
requests and their review threads.

GitHub is built into latchkey; the browser flow creates a personal
access token during login:

```sh
latchkey auth browser github
```

## GitLab

`type = "gitlab"` — web API through latchkey (`api`). Mirrors merge
requests and their discussions.

GitLab is built into latchkey too, but takes a personal access token
you create yourself (User settings → Access tokens, with API read
access):

```sh
latchkey auth set gitlab -H "PRIVATE-TOKEN: $(pbpaste)"
```

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

Use a `gmail` table on the ingest step. Incremental sync is driven by
Gmail's own change history, so deletions and label changes show up as
events. Throughput is capped by Google's quota at roughly 300 messages
a minute, so a large mailbox backfills over several runs. A Takeout
`.mbox` of the same mailbox is the no-credentials route — see
[Email](#email).

## Google Takeout

`type = "google_takeout"` — an unpacked Takeout tree on disk
(`export`). Mirrors Google Chat and Voice messages (rendered to
markdown); Maps reviews, saved places and photos, YouTube watch history
and subscriptions, and Gemini Apps activity (extracted to the raw
store, not yet rendered).

Self-service export at <https://takeout.google.com>. Deselect all, then
tick just what you want, request a `.zip`, and unpack it:

```sh
unzip ~/Downloads/takeout-*.zip -d ~/backups/
```

Useful products: **Chat**, **Voice**, **Maps**, **YouTube history** and
**Gemini** (read by this source from the unpacked tree), and **Mail**
(a single `.mbox`, read by the [email](#email) source instead). A
Takeout is a complete snapshot, so it is also the way to notice what
Google has deleted since the last one.

## Lightroom

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

## LinkedIn

`type = "linkedin"` — LinkedIn's "Get a copy of your data" export
(`export`). Mirrors messages, and connections as contacts.

**Settings & Privacy** → **Data privacy** → **Get a copy of your
data**, request the full archive, and unzip it when the email arrives
(it can take a day):

```sh
unzip ~/Downloads/Complete_LinkedInDataExport_*.zip -d ~/backups/LinkedInDataExport
```

Point `export.path` at that directory. Each export is complete, so the
sample config sets `always_clear_before_ingest = true` to let a newer
export drop what LinkedIn stopped including.

## Local files

`type = "fsindex"` — any directory tree on disk (`fswalk.path`).
Mirrors an index of every entry — path, kind, size, blake3 hash;
download-only, nothing is rendered.

Nothing but a path. The scan is incremental, keyed on mtime, size and
inode, so a rescan of a big tree is fast, and it stays read-only
against its input unless you turn `stamp` on. Query the index in the
source's raw store.

## Media

`type = "media"` — a directory tree of music, photos and video
(`fswalk.path`). Mirrors every audio, image and video file with its
metadata — artist, album and track for music; camera, lens, exposure,
GPS and capture time for photos and video — plus `.m3u` playlists;
download-only, nothing is rendered.

Nothing but a path. Files are keyed on content hash, not path, so the
same song synced to three folders is one row with three locations, and
a second hash over just the signal (`payload_blake3`) survives
retagging. Query the raw store directly; the provider's `INGEST.md`
has the recipes.

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

Every request also needs a `Notion-Version` header, and the client
deliberately sends neither the token nor the version — latchkey injects
both — so the stored credential must carry the version too. Set both
headers in one `auth set`:

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
(page ids or paste-able URLs); everything under them comes along.

## PDFs

`type = "pdf"` — a directory tree on disk (`fswalk.path`). Mirrors
every PDF under it, converted to markdown and keyed on content hash, so
PDFs show up in the grid and in search.

Nothing but a path. Two copies of the same paper convert once and share
one row; renaming or moving a PDF preserves its identity. No OCR yet: a
scanned, image-only PDF is recorded (`needs_ocr = 1` in the source's
`pdf_documents` table) and produces no text.

## Perseus

`type = "perseus"` — a public download (`github`). Mirrors TEI
editions of Greek and Latin texts from the Perseus Digital Library.

No credentials. An empty `github` table pulls the default Thucydides
pair from PerseusDL's `canonical-greekLit`; `files` picks others. Since
it needs nothing of yours, it is a good first sync to check an install
with.

## Signal

`type = "signal"` — an Android backup file (`backup.path`). Mirrors
messages and media.

Signal stores encrypted backups on the phone. Enable backups in the app
(Settings → Chats → Backups), then pull the backup directory off the
device over `adb`:

```sh
adb pull /sdcard/Signal/SignalBackups ~/backups/SignalBackups
```

You'll also need the 30-digit passphrase shown when you enabled backups.
The provider reads it from the `SIGNAL_BACKUP_PASSPHRASE` env var (override
per source with `aep_env_var`).

## Slack

`type = "slack"` — web API through latchkey (`api`). Mirrors channels,
DMs and file attachments.

Built into latchkey — no manual export:

```sh
latchkey auth browser slack
```

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
newer export deduplicates against what's already there.

## WhatsApp

`type = "whatsapp"` — an Android `crypt15` backup (`backup.path`).
Mirrors messages and media.

The provider ingests the end-to-end-encrypted `msgstore.db.crypt15`
database — the newest backup format, and the only one we support. The
older password-based backups are *not* decryptable offline, so don't use
that path.

**Get the key.** This is the part that usually trips people up, and no
root is needed. In WhatsApp, go to Settings → Chats → Chat backup →
End-to-end encrypted backup. Turn it on and choose the **64-digit key**
option (not a password). Write that key down — that *is* the key. If you
already enabled E2EE with a password, turn it off and re-enable with the
64-digit option, or you'll be stuck.

The provider reads the 64-digit hex key from the
`WHATSAPP_BACKUP_DECRYPTION_KEY` env var (override per source with
`key_env_var`).

**Pull the encrypted database.** Trigger a fresh local backup first
(Settings → Chats → Chat backup → Back Up) so the file is current, then
plug the phone in with USB debugging on and pull it off over `adb`:

```sh
adb pull /sdcard/Android/media/com.whatsapp/WhatsApp/Databases/msgstore.db.crypt15 .
```

Or copy it through MTP / a file manager. To also bring over media, pull
the whole backup directory instead:

```sh
adb pull /sdcard/Android/media/com.whatsapp/WhatsApp/ ~/backups/WhatsApp/
```

The whole decrypted database is kept — every table, versioned across
backups, the way the Lightroom and Apple Photos sources keep theirs —
so a message you delete on the phone is still in an earlier commit of
the store. Three tables of app bookkeeping are left out by default
(`skip_churn`); `providers/whatsapp/INGEST.md` says which and why.

## YoLink

`type = "yolink"` — YoLink's API (`api`). Mirrors per-device sensor
history — temperature, humidity, water — rendered as one page of
interactive plots.

Each device is one `devices` entry naming its `kind`
(`temperature_humidity` or `watermeter`), the `start` date to mirror
from, and two 32-hex-character ids, `family_device_id` and
`device_udid`. The ids are effectively read secrets that cannot be
rotated, and they go straight into the config — keep that file to
yourself. `all_sources.toml` shows the shape.
