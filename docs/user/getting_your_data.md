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

WhatsApp keeps its media and message databases under app storage on the
phone. Pull them off the device over `adb`:

```sh
adb pull /sdcard/Android/media/com.whatsapp/WhatsApp/ ~/backups/WhatsApp/
```

You'll also need the 64-character hex backup key. The provider reads it
from the `WHATSAPP_BACKUP_DECRYPTION_KEY` env var (override per source with
`key_env_var`).

## Other sources

These are wired into the pipeline; acquisition is via `latchkey` or a
provider export. Fill in details as we use them: GitHub, GitLab, Notion,
Beeper, Contacts, generic email (JMAP).
