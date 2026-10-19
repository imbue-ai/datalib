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

The rules, in `src/ingest/backups.rs` (pure) and `src/ingest/mod.rs`:

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
snapshot already stored is not decrypted again. The shared tree holds
what Signal keeps now, not what each old snapshot named, so a first
sync over an old folder can find an old snapshot's attachment missing:
that is a problem on its edge, the same as any attachment the backup
holds no bytes for.
