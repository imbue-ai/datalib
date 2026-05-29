# Beeper provider — Extract

Beeper is a Matrix homeserver at `matrix.beeper.com`. The user's
account is the multiplex point: server-side bridges relay iMessage,
WhatsApp, Signal, Telegram, Discord, LinkedIn, … into Matrix rooms.
We talk to it via the standard Matrix Client-Server API; per-bridge
semantics are deferred to the Translate stage.

## Auth setup

Beeper issues a long-lived Matrix `access_token`. Latchkey injects it
as `Authorization: Bearer <token>` on every request the `beeper`
provider makes.

### Option 1 — pull the token from the on-disk SQLite (fastest)

Beeper Texts stores the Matrix access token in plaintext in a
SQLite file under Application Support. Sign in to the desktop app
once, then run:

```sh
# One-time service registration (tells latchkey to inject the bearer
# token on requests to matrix.beeper.com):
latchkey services register beeper --base-api-url="https://matrix.beeper.com/"

# Copy the access token to the clipboard (never echoed):
sqlite3 ~/Library/Application\ Support/BeeperTexts/account.db \
    "SELECT access_token FROM account;" | pbcopy

# Register it with latchkey ($(pbpaste) is recorded literally in
# shell history, so the token itself never lands there):
latchkey auth set beeper -H "Authorization: Bearer $(pbpaste)"

# Smoke-test:
latchkey curl -s https://matrix.beeper.com/_matrix/client/v3/account/whoami
# Expect: {"user_id":"@you:beeper.com","device_id":"..."}
```

> **Threat-model note.** The Beeper Texts SQLite stores the token
> as plaintext at filesystem mode 0644 — any process running as you
> can read it. That's the same exposure latchkey's own cookie store
> has on the same machine (see the warning at the top of
> `docs/first_time_user.md`), but it does mean rotating the token
> requires signing out of the desktop app — deleting it from
> latchkey alone won't help. A future `beeper-login` helper
> (Milestone E) is just a thin wrapper around the two commands above.

### Option 2 — pull the token from the desktop app's DevTools

Only useful if you can't read the SQLite (different OS, sandboxed
install, etc.). The token is **not** in `localStorage` — that
holds only UI state. It lives in IndexedDB or the SQLite store
the app uses for crypto/account data. In Beeper Texts hit `⌘ ⌥ I`
to open DevTools, switch to the **Application** tab → **Storage**
→ **IndexedDB**, and look through the `matrix-js-sdk:*` stores for
an object carrying an `accessToken` field. Copy that value into
the same `latchkey auth set beeper …` command above.

## What lands on disk

`beeper-download --out <path>` (or `frankweiler-sync` with a
`beeper_api` source) writes a single doltlite file at
`<data_root>/raw/<name>.doltlite_db` with three object tables:

- `rooms`  — one row per joined Matrix room, plus an inferred
  `bridge_network` column derived from the room's `m.bridge` state
  event (with fallback to the localpart prefix of any bridge-bot
  member, e.g. `@imessagebot:beeper.local` → `imessage`).
- `users`  — one row per Matrix user we've seen, with the bridge-side
  native id (phone number, iMessage handle, etc.) parsed out of the
  mxid localpart and stored as `remote_id`.
- `events` — Milestone B: one row per Matrix event, captured via
  `/_matrix/client/v3/rooms/{id}/messages?dir=b`.

Plus the shared `blobs`, `endpoint_shapes`, and `sync_runs` tables
that every doltlite raw store carries.

## Encrypted rooms

Matrix-native (non-bridged) rooms are end-to-end encrypted with
Megolm. Beeper's bridges decrypt server-side and re-emit cleartext
events for bridged rooms, so iMessage / WhatsApp / Signal / etc.
arrive in plaintext. The Beeper provider ignores
`m.room.encrypted` events without trying to decrypt them — that's
deferred until/unless we genuinely need to read Matrix-native chats.

## Filtering scopes

The `BeeperApiSync` config block (in
`frankweiler-core::config::BeeperApiSync`) carries:

- `networks: ["imessage", "signal"]` — restrict to specific bridges
  (matched against the inferred `bridge_network`).
- `rooms: ["!abc:beeper.com", "https://matrix.to/#/!xyz:beeper.com"]`
  — explicit Matrix room IDs (or paste-able matrix.to URLs); skips
  the `networks` filter when non-empty.
- `refresh_window_days: 14` — Milestone B knob for the trailing-edits
  re-walk.
- `media: true` — Milestone B knob for `mxc://` attachment fetches.
