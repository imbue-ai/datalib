# The `google_takeout` source

An unpacked [Google Takeout](https://takeout.google.com) export, read
off `common.input_path`. There is no API and no network: the download
step walks a directory tree the user exported and unzipped themselves.

## Every feed is opt-in, and off by default

`SyncFlags` (`src/download/mod.rs`) is one boolean per feed, and
`Default` sets every one of them to `false`. A user has to enable each
feed consciously.

That is deliberate, and it is the one rule to preserve if you touch
this provider. A Takeout export is whatever the user asked Google for,
so its subtrees are wildly uneven — a export may hold nine years of
YouTube history and no Maps data at all — and silently ingesting
everything present would make "add this source" an unbounded promise.
`google_voice_include_spam` is a second-level switch under
`google_voice` for the same reason: spam is bulky and only useful for
parser hardening.

`fixture_walk.rs`'s `defaults_ingest_nothing` is the regression test —
it runs the walk with a default `SyncFlags` and asserts every feed
lands zero rows.

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

`DATA_TABLES` and `EDGE_TABLES` in `src/download/schema_raw.rs` are the
authoritative lists — the table above is a reader's summary of them,
and they are what `reset` wipes. Google Voice keeps its own
`schema_raw` under `src/download/google_voice/`.

Ids are uuidv5 under a per-provider namespace, minted from a recipe
string (`"maps_review:{ftid}:{date}"`, `"youtube:watch:{id}:{ts}"`);
see `ns_id` in `schema_raw.rs` and
[`docs/dev/entity_ids.md`](../../../../../docs/dev/entity_ids.md).

## Tests

`tests/fixture_walk.rs` points the downloader at the checked-in
TNG-themed Takeout tree and asserts, per feed, the rows that land —
counts, a sample of the parsed content, the CAS digest for the photo
feed, and that a second walk over an unchanged tree ingests nothing.
