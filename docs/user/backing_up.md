# Backing up a library

A library is one folder, the data root (`~/datalib` in these examples).
Everything Datalib knows about it is in files under that folder, so a
backup is a copy of the folder, and a restore is putting the copy back.

## Copy it while nothing runs

Quit the desktop app, or stop `datalib-http` and any `datalib-pipeline`
you started, before you copy. A sync writes its stores as it goes, and a
copy taken in the middle of one is not something we test.

## The whole folder

The simplest backup is all of it:

```sh
cp -R ~/datalib /Volumes/Backup/datalib
```

A backup tool works the same way. Nothing in a library is marked as a
cache, so `restic backup --exclude-caches`, `borg create
--exclude-caches` and `tar --exclude-caches` copy every file of it.

What a copy does when it comes back:

- **At another path**, it works as it is. Nothing in a library names
  the folder it lives in, and the first sync there redoes nothing.
- **Put back after newer syncs**, it is behind, and one sync catches it
  up: each source downloads what upstream has now, and the rest is
  rebuilt from that.

Earlier builds marked each source's `render_markdown/` and the
`unified_grid_and_search_index/` folder as caches, and the first start
of a later build removes those marks. A backup made with
`--exclude-caches` by one of them has neither, but does have `system/`.
Restore it without `system/`, as the next section says, or the library
comes back with an empty search.

## A smaller backup: what nothing rebuilds

Most of a library's size is rebuilt by a sync from the rest. To keep
only what cannot be rebuilt, back up:

- `config.toml`, the sources and their settings;
- every `<source>/ingest/` folder: what each source downloaded, which
  may hold things upstream has since deleted;
- `datalib_curated/`: your contacts, and the problems you muted;
- `system/frontend/user/`, if you or an agent wrote components there;
- the input folder of a source that reads files where they lie. Such a
  source keeps a fingerprint of each file rather than its bytes, and
  reads the file itself when it renders. The PDF source is one.

Leave out everything else **including all of `system/`**. `system/`
records which steps are up to date. Put back without the folders it
describes, it says they are still there, and the sync rebuilds nothing.
Leaving it out costs the sync history and logs, the remote media you
allowed a document to load, and which steps you turned off.

Restore by copying those back into an empty folder and syncing. The
renders and the search index are rebuilt. On a large library that takes
a while, longer still for a source searched by meaning, since every
passage is embedded again. A source whose upstream is gone, such as an
export you no longer have, fails its download on each sync and keeps
what it has, and everything that reads it still works. To stop the
failures, set `download = false` under the download step's
`[steps.params.common]`; the source then keeps what it has without
trying ([A source that no longer downloads](../dev/config_model.md#a-source-that-no-longer-downloads)).

## What is tested

Datalib's tests (`copy_restore.rs`) sync a small library and
copies it each way above: whole to a new path, an older copy put back,
only what nothing rebuilds, and through a tool that skips caches. Each
restored copy has to answer the same searches, `from:me` and contacts
as the original. Two things are not tested: a copy taken while a sync
runs, and a copy moved to another machine.
