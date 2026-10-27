# signal — a folder of Android backups, one commit each

Signal Android's local backup writes a new snapshot every day into one
folder: `signal-backup-YYYY-MM-DD-HH-MM-SS/`, each holding `metadata`,
`main` (the encrypted frame stream) and `files` (the list of
attachments it needs), beside one shared `files/XX/<name>` tree of
encrypted attachments. Point `backup.path` at that folder.

Each sync reads every snapshot the store does not hold, oldest first,
and commits each on its own, dated when Signal took it (the
`backupTimeMs` in the snapshot's `BackupInfo`). A snapshot is the
whole of the account, so each one read whole replaces what the store
held: a message it no longer has is deleted in its commit, and one it
says something else about is modified. Any two commits therefore diff
like two days' backups, which is the point — a message lost or edited
between two backups shows up as a `removed` or `modified` row:

```sh
datalib-doltlite -readonly signal/ingest/entities.doltlite_db \
  "SELECT diff_type, coalesce(to_id, from_id)
     FROM dolt_diff_chat_items('HEAD^1', 'HEAD')"
```

(`docs/dev/doltlite.md` §"What the upstream deleted on you" has the
recipes; stock `sqlite3` cannot open the store.)

The rules, in `src/ingest/backups.rs` and `src/ingest/keys.rs` (pure)
and `src/ingest/mod.rs`:

- **A row is keyed by who it is about, not by the backup's numbers.**
  A backup numbers its recipients and chats itself, and a phone restored
  or transferred from another numbers them anew. So a recipient is keyed
  by their ACI, else their PNI, else their number (`aci:<hex>`,
  `pni:<hex>`, `e164:+<n>`), a group by a hash of its master key
  (`group:<hash>`), and the account by `self`. A chat takes the key of
  the recipient it is with, and a message is `<chat>#<author>#<date_sent>`.
  A phone switch then adds and removes nothing. Rows whose payload holds
  the backup's own numbers read as modified. A recipient with nothing
  stable, or a second one claiming a key already taken, is keyed by its
  number (`recipient:7`, `chat:12`), which holds for that backup only.
  A contact first seen by number alone changes key once, in the first
  backup that carries their ACI.

- **A snapshot is known by its bytes.** The ledger, `ingested_backups`,
  is keyed by the blake3 of the snapshot's three files, read through
  the host's fingerprint cache, so an unchanged folder costs a `stat`
  per file and decrypts nothing. A folder renamed or copied is the
  snapshot already held.
- **A row's sidecar is stamped the first time a snapshot holds it**
  (`bulk_upsert_first_seen_in_tx`), so a day's commit holds what that
  day changed and not a re-stamp of every row.
- **HEAD always ends on the newest snapshot.** The ledger row written
  last names the snapshot HEAD holds. When a run reads an older
  snapshot that turned up late, the newest is read again on top, dated
  now; when HEAD already holds it, nothing more is read.
- **The newest snapshot held, gone from the folder, leaves HEAD
  alone.** Reading an older one on top would delete what only the
  newer one held, and that would read as data loss. It is a problem on
  the Manage row until a newer snapshot arrives.
- **A snapshot that will not open costs that snapshot.** One half
  synced (a file missing) or corrupt is a problem keyed by its name,
  and is tried again next run; the rest are read. When every snapshot
  a run tried fails — most likely a passphrase that is not this
  account's — the run fails instead.
- **A frame that would not decode** means its snapshot was not read
  whole: it deletes nothing, and the `phase:frames` problem describes
  the last snapshot read, which is what HEAD holds.
- **A stop is honoured between snapshots**; one begun is read whole.

Attachments are decrypted into the blob CAS once: one an earlier
snapshot already stored is not decrypted again. A snapshot that holds
an attachment without its bytes keeps the bytes an earlier one stored,
and nothing is reported missing. This happens after a phone switch,
since the new phone has not downloaded what the old one had, and on an
old snapshot whose file the shared tree no longer holds. Render finds
the bytes through the attachment's edge row, never through the
message's payload. An attachment no snapshot held bytes for is a
problem on its edge.
