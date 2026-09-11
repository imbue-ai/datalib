# Getting your data

A per-source cheat sheet for getting a copy of your data where the
pipeline can read it: either credentials stored in `latchkey`, or an
export on disk. This doc is only about *access* — for the config file
and running the sync, see the
[first-time user guide](first_time_user.md).

Conventions: exports land under `~/backups/`, and `latchkey` runs
through `npx` so there is nothing to install. Adjust paths to taste and
point the matching source in your config at them. Wherever a command
takes a secret, it is written as `$(pbpaste)`: copy the secret to your
clipboard, then run the command. Your shell history keeps the harmless
`$(pbpaste)` text rather than the secret itself.

The credentials `latchkey` stores are the same session your browser
has. Anything that can run commands as you can use them to act as you
on that service — see the warning in the first-time guide before
storing any.

## Google Takeout

Self-service export at <https://takeout.google.com>. Deselect all, then
tick just what you want, request a `.zip`, and unpack it:

```sh
unzip ~/Downloads/takeout-*.zip -d ~/backups/
```

Useful products: **Mail** (a single `.mbox`, read by the `email`
source), and **Chat**, **Voice**, **Maps**, **YouTube history** and
**Gemini** (read by the `google_takeout` source from the unpacked
tree). A Takeout is a complete snapshot, so it is also the way to notice
what Google has deleted since the last one.

## Gmail (live, over the API)

The least setup of any web source. Gmail is built into latchkey; one
command opens a browser, you sign in and approve every scope it asks
for, and latchkey keeps the OAuth token:

```sh
npx -y latchkey auth browser google-gmail
```

Use the `email` source with a `gmail` table on its ingest step. Incremental sync is
driven by Gmail's own change history, so deletions and label changes
show up as events. Throughput is capped by Google's quota at roughly
300 messages a minute, so a large mailbox backfills over several runs.

## Slack

Built into latchkey — no manual export:

```sh
npx -y latchkey auth browser slack
```

## Claude.ai

`claude.ai` has no official API for your conversations, so this uses
the session cookie your browser already has. Register the service once,
then stage the cookie command:

```sh
npx -y latchkey services register claude-ai --base-api-url="https://claude.ai/"
npx -y latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
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
table pointing at the unpacked folder instead.

## ChatGPT

A one-time registration, then a browser login. ChatGPT uses a bearer
access token rather than a cookie, and latchkey can go and fetch it
for you. The app's Add Data Source wizard does both from its
**Latchkey auth** button (and **Test connection** then lists the
account's conversations to pick from); by hand it is:

```sh
npx -y latchkey services register chatgpt \
  --base-api-url="https://chatgpt.com/" \
  --login-url="https://chatgpt.com/auth/login" \
  --login-flow=token-capture \
  --login-flow-params='{"tokenUrl": "https://chatgpt.com/api/auth/session", "tokenField": "accessToken"}'
npx -y latchkey auth browser chatgpt
```

The second command opens chatgpt.com, waits for you to log in, and
stores the token itself — nothing to copy or paste. The token rotates
frequently; when `latchkey services info chatgpt` reports `invalid` or
a sync comes back `HTTP 401 token_expired`, run that same
`auth browser` line again.

If you registered `chatgpt` before this guide said to, latchkey will
have recorded it as a `set`-only service — `latchkey services info
chatgpt` shows `authOptions` without `browser`, and a name that
already exists cannot be re-registered. Run `npx -y latchkey services
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
npx -y latchkey auth set chatgpt -H "Authorization: Bearer $(pbpaste)"
```

As with Claude.ai, the impersonating curl clears Cloudflare, so no
`cf_clearance` cookie is needed.

## Fastmail

Built into latchkey, including the region-prefixed API hosts
(`phl.api.fastmail.com` and the like) that an account homed in a
regional datacenter gets. The browser flow stores an OAuth token:

```sh
npx -y latchkey auth browser fastmail
```

If you would rather use an API token, create one at
[app.fastmail.com/settings/security](https://app.fastmail.com/settings/security)
under **Integrations** → **API tokens** → **New API token**, give it
read access to your mail, copy it, and store it instead:

```sh
npx -y latchkey auth set fastmail -H "Authorization: Bearer $(pbpaste)"
```

Use the JMAP mode of the `email` source with `hostname =
"api.fastmail.com"`. The same service works for any other JMAP server
if you register its host; Fastmail is the one that is built in.

## Contacts

Two routes into the `contacts` source, one table each:

- **A `.vcf` export.** Most address books export vCards; point
  `vcf.path` at a directory of them. No credentials.
- **A CardDAV server** (a `carddav` table). Credentials go in latchkey under a service
  whose base URL matches the server. Fastmail's is built in and takes
  an app password (Settings → Privacy & Security → Integrations → App
  passwords, with contacts access):

  ```sh
  npx -y latchkey auth set fastmail-dav -u "you@fastmail.com:$(pbpaste)"
  ```

## GitHub and GitLab

GitHub is built into latchkey; the browser flow creates a personal
access token during login:

```sh
npx -y latchkey auth browser github
```

GitLab is built in too, but takes a personal access token you create
yourself (User settings → Access tokens, with API read access):

```sh
npx -y latchkey auth set gitlab -H "PRIVATE-TOKEN: $(pbpaste)"
```

## Notion

Notion authenticates with an **internal integration** token. Create one
at [notion.so/my-integrations](https://www.notion.so/my-integrations) →
**New integration**, associate it with your workspace, give it read
capabilities, and copy the **Internal Integration Secret**.

Two things about Notion trip people up, and both fail in ways that don't
look like credential problems:

**1. The integration starts with access to nothing.** A token is not
enough — Notion scopes access per page. In Notion, open each page (or
top-level page of a subtree) you want mirrored, use the **⋯** menu →
**Connections** → **Connect to**, and pick your integration. Access is
inherited by child pages, so connecting the root of a subtree is enough.
Skip this and the API returns `404 object_not_found` for a page you can
plainly see in the app.

**2. Every request needs a `Notion-Version` header.** The client
deliberately sends neither the bearer token nor the version — latchkey
injects both — so the credential must carry the version too. Set both
headers in one `auth set`:

```sh
npx -y latchkey auth set notion \
  -H "Authorization: Bearer $(pbpaste)" \
  -H "Notion-Version: 2022-06-28"
```

Omitting the version header gets every request rejected with
`400 missing_version`. Verify the whole path — token, version header,
and page access — in one call before running a sync:

```sh
npx -y latchkey curl "https://api.notion.com/v1/pages/<page-id>"
```

A `200` with a JSON page body means you're set. `400 missing_version`
means the version header is missing from the credential;
`404 object_not_found` means the page hasn't been connected to the
integration.

Point the source's `subtrees.pages` at the page URLs you connected.

## Signal

Signal stores encrypted backups on the phone. Enable backups in the app
(Settings → Chats → Backups), then pull the backup directory off the
device over `adb`:

```sh
adb pull /sdcard/Signal/SignalBackups ~/backups/SignalBackups
```

You'll also need the 30-digit passphrase shown when you enabled backups.
The provider reads it from the `SIGNAL_BACKUP_PASSPHRASE` env var (override
per source with `aep_env_var`).

## WhatsApp

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

## SMS & calls (SMS Backup & Restore)

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

## LinkedIn

LinkedIn's own export: **Settings & Privacy** → **Data privacy** →
**Get a copy of your data**, request the full archive, and unzip it
when the email arrives (it can take a day):

```sh
unzip ~/Downloads/Complete_LinkedInDataExport_*.zip -d ~/backups/LinkedInDataExport
```

Point `export.path` at that directory. Each export is complete,
so the sample config sets `always_clear_before_ingest = true` to let a
newer export drop what LinkedIn stopped including.

## Beeper

Reads the Beeper Texts desktop app's local data directory (on macOS,
`~/Library/Application Support/BeeperTexts`); no credentials. Lightly
used — expect rough edges.

## Files already on your disk

These sources need nothing but a path on their ingest step:

- **`pdf`** — `fswalk.path`, a directory tree; every PDF under it is converted to
  markdown (no OCR yet, so image-only scans are recorded but produce no
  text).
- **`media`** — `fswalk.path`, a directory tree of music, photos and video.
- **`fsindex`** — `fswalk.path`, any directory tree, indexed by path.
- **`lightroom`** — an Adobe Lightroom Classic `.lrcat` catalog (see its entry in `all_sources.toml` for the table name).
- **`apple_photos`** — `library.path`, an Apple Photos `.photoslibrary`. On
  macOS the library is a protected location: in the app, choose it with
  the picker rather than typing the path, and if a sync still reports
  "Operation not permitted", grant Datalib Full Disk Access in System
  Settings → Privacy & Security.

## Other sources

**YoLink** is configured with per-device ids that are effectively
read secrets, written straight into the config; **Perseus** downloads
public texts and needs no credentials. Both are documented in
[`all_sources.toml`](config_examples/all_sources.toml).
