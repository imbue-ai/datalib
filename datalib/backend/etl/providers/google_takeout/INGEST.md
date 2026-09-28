# The `google_takeout` source

An unpacked [Google Takeout](https://takeout.google.com) export, read
off `export.path` (the directory holding `Takeout/`); the feed switches
sit beside it in the same table. There is no API and no network: the
download step walks a directory tree the user exported and unzipped
themselves.

## Every feed is opt-in, and off by default

`SyncFlags` (`src/ingest/mod.rs`) is one boolean per feed, mirrored by
the `export` table in `google_takeout_config`, and `Default` sets every
one of them to `false`. A user has to enable each
feed consciously.

That is deliberate, and it is the one rule to preserve if you touch
this provider. A Takeout export is whatever the user asked Google for,
so its subtrees are wildly uneven — an export may hold nine years of
YouTube history and no Maps data at all — and silently ingesting
everything present would make "add this source" an unbounded promise.
`google_voice_include_spam` is a second-level switch under
`google_voice` for the same reason: spam is bulky and only useful for
parser hardening.

`tests/fixture_walk.rs`'s `sync_flags_default_disables_everything` is
the regression test: it runs the walk with a default `SyncFlags` and
asserts that the Maps, YouTube, Chat and Gemini feeds land nothing.

## The feeds and what they write

| flag | raw tables |
|---|---|
| `maps_reviews` | `maps_reviews` |
| `maps_saved_places` | `maps_saved_places` |
| `maps_photos` | `maps_photos` (bytes in the blob CAS) |
| `youtube_watch_history` | `youtube_watch_history` |
| `youtube_subscriptions` | `youtube_subscriptions` |
| `google_chat` | `chat_groups`, `chat_users`, `chat_messages`, `chat_attachments` |
| `gemini_apps` | `gemini_activity`, `gemini_attachments` |
| `google_voice` | `voice_messages`, `voice_bills`, `voice_greetings`, `voice_attachments` |

`DATA_TABLES` and `EDGE_TABLES` in `src/ingest/schema_raw.rs` list the
tables the store is created with. Google Voice keeps its own
`schema_raw` under `src/ingest/google_voice/`.

Ids are uuidv5 under a per-provider namespace, minted from a recipe
string (`"maps_review:{ftid}:{date}"`, `"youtube:watch:{id}:{ts}"`);
see `ns_id` in `schema_raw.rs` and
[`docs/dev/entity_ids.md`](../../../../../docs/dev/entity_ids.md).

## Tests

`tests/fixture_walk.rs` points the downloader at the checked-in
TNG-themed Takeout tree (`tests/fixtures/Takeout/`) and asserts, per
Maps, YouTube, Chat and Gemini feed, the rows that land — counts, a
sample of the parsed content, the CAS digest for the photo feed — and
that a second walk over an unchanged tree ingests nothing. Google
Voice's parser and walk are tested in `src/ingest/google_voice/`.
