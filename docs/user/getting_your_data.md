# Getting your data

A short, per-source cheat sheet for how to get a copy of your data onto
disk so the sync pipeline can ingest it. This doc is just about
*acquisition* — for credential setup, config, and running the sync, see
the [first-time user guide](/docs/user/first_time_user.md).

Conventions below: exports land under `~/backups/`. Adjust paths to taste
and point the matching source stanza in your config at them.

## Google Takeout

Self-service export at <https://takeout.google.com>. Deselect all, then
tick just what you want, request a `.zip`, and unpack it:

```sh
unzip ~/Downloads/takeout-*.zip -d ~/backups/
```

Useful products: **Mail** (a single `.mbox`, ingested directly),
**Chat**, **Maps (Your Timeline)**, **YouTube history**, **Gemini**.

## Slack

Credentials are captured at runtime via `latchkey` — no manual export:

```sh
npx -y latchkey auth browser slack
```

## Claude web (Anthropic)

`claude.ai` is not a built-in latchkey service, so it needs a one-time
custom registration before credentials can be set. Register the service,
then stage the cookie command (`$(pbpaste)` keeps the live token out of
your shell history):

```sh
npx -y latchkey services register claude-ai --base-api-url="https://claude.ai/"
npx -y latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
```

Open [claude.ai](https://claude.ai) in a logged-in tab and copy your
`sessionKey` cookie — it's `HttpOnly`, so read it from DevTools →
**Application** → **Storage** → **Cookies** → `https://claude.ai`, find
the `sessionKey` row, and copy its **Value**. With it on the clipboard,
run the staged `auth set` command. See the "Register Claude web with
latchkey" steps in the [first-time guide](/docs/user/first_time_user.md)
for the full walkthrough.

The downloader fetches over `latchkey curl` and clears Cloudflare's
managed challenge via the in-tree Chrome-impersonating shim, so no
`cf_clearance` cookie is needed — the `sessionKey` cookie is the entire
auth surface.

## ChatGPT (OpenAI)

`chatgpt.com` is not a built-in latchkey service either, so it needs a
one-time custom registration. It authenticates with a bearer access
token (not a cookie):

```sh
npx -y latchkey services register chatgpt --base-api-url="https://chatgpt.com/"
npx -y latchkey auth set chatgpt -H "Authorization: Bearer $(pbpaste)"
```

ChatGPT doesn't expose the token as a readable cookie — grab it from a
logged-in tab via DevTools → **Console**:

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

Click anywhere on the page to copy the token, then run the staged
`auth set` command. The token rotates frequently — when `latchkey
services info chatgpt` reports `invalid` or requests come back `HTTP
401 token_expired`, re-run the console snippet and `auth set`. As with
Claude, the Chrome-impersonating shim clears Cloudflare, so no
`cf_clearance` cookie is needed.

## Hermes Agent / OpenClaw local agent history

No account export or credentials are required when the history is already on
this machine. Add a `hermes` source with managed local sync:

```yaml
sources:
  - name: hermes
    source:
      type: hermes
      sync: {}
```

With the empty `sync` block, datalib discovers local agent roots such as
`~/.hermes`, `~/.hermes/profiles/*`, and OpenClaw-compatible roots when they
exist. It reads each `state.db` read-only and folds in legacy session JSON;
the source database files are not copied or mutated.

The rendered datalib output is still sensitive: it mirrors transcript contents
(system prompts, memory, reasoning, tool calls, and tool output) into your
`data_root` as Markdown and index rows. Treat both the local agent roots and
the datalib `data_root` as private data.

If the history lives somewhere non-standard or was copied from another machine,
use the explicit export-directory fallback instead:

```yaml
sources:
  - name: hermes-export
    source:
      type: hermes
      common:
        input_path: ~/backups/hermes-export
```

## Fastmail

`fastmail` isn't a built-in latchkey service, so it needs a one-time
custom registration. It serves the JMAP API from `api.fastmail.com` and
downloads (attachments, blobs) from `fastmailusercontent.com`. Latchkey
routes by URL host but `register` only takes one `--base-api-url` per
service, so register the two hosts as two services:

```sh
npx -y latchkey services register fastmail \
  --base-api-url="https://api.fastmail.com/"
npx -y latchkey services register fastmail-content \
  --base-api-url="https://www.fastmailusercontent.com/"
```

Fastmail authenticates with an API token (a bearer token, not a
password). Create one at
[app.fastmail.com/settings/security](https://app.fastmail.com/settings/security)
under **Integrations** → **API tokens** → **New API token**, give it
read access to your mail, and copy it. Then attach the same token to
both services (`$(pbpaste)` keeps the live token out of your shell
history):

```sh
npx -y latchkey auth set fastmail         -H "Authorization: Bearer $(pbpaste)"
npx -y latchkey auth set fastmail-content -H "Authorization: Bearer $(pbpaste)"
```

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
the source's `input_path` at that directory — it walks every `*.xml`
inside, so keeping multiple dated backups there is fine; re-ingesting a
newer export deduplicates against what's already there.

## Other sources

These are wired into the pipeline; acquisition is via `latchkey` or a
provider export. Fill in details as we use them: GitHub, GitLab, Notion,
Beeper, Contacts, generic email (JMAP).
