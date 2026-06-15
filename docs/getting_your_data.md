# Getting your data

A short, per-source cheat sheet for how to get a copy of your data onto
disk so the sync pipeline can ingest it. This doc is just about
*acquisition* — for credential setup, config, and running the sync, see
the [first-time user guide](first_time_user.md).

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

Captured via `latchkey` by pasting your `claude.ai` `sessionKey` cookie.
See the "Register Claude web with latchkey" steps in the first-time
guide.

## Signal

Signal stores encrypted backups on the phone. Enable backups in the app
(Settings → Chats → Backups), then pull the backup directory off the
device over `adb`:

```sh
adb pull /sdcard/Signal/SignalBackups ~/backups/SignalBackups
```

You'll also need the 30-digit passphrase shown when you enabled backups.

## WhatsApp

WhatsApp keeps its media and message databases under app storage on the
phone. Pull them off the device over `adb`:

```sh
adb pull /sdcard/Android/media/com.whatsapp/WhatsApp/ ~/backups/WhatsApp/
```

## Other sources

These are wired into the pipeline; acquisition is via `latchkey` or a
provider export. Fill in details as we use them: GitHub, GitLab, Notion,
Beeper, Contacts, ChatGPT, generic email (JMAP).
