# Should a render store's rows be keyed by their document?

The question: `grid_rows` is keyed by each row's own `uuid`, time-ordered
(`docs/dev/entity_ids.md` § "The layout"), with an index on
`markdown_uuid`. Every re-render of a document deletes and upserts its
rows by `markdown_uuid`. A profile of a Slack re-render of 62k documents
in a 9.8 GB render store spent most of the writer's time reading pages
from disk, in seeks from an index to a row. Keying rows by
`(markdown_uuid, uuid)` would put a document's rows side by side.

`scripts/doltlite_key_by_document.py` builds the same rows under each
layout against the Bazel-built doltlite CLI and times, with the file's
pages evicted from the page cache before each run, a read of every row of
5,000 random documents and the render's write for them. The rows are
modelled on a real Slack render store: in it, 84% of 135,502 documents
keep all their rows within an hour of each other, 10% within a day, 6%
longer, about 4.3 rows a document. What the numbers mean is
[`docs/dev/doltlite.md`](../../docs/dev/doltlite.md) § "What a write
costs"; this directory is the reproduction.

```sh
bazelisk build //third-party/doltlite:doltlite
uv run python scripts/doltlite_key_by_document.py [scratch_dir] [documents]
```

## Results — doltlite 0.51.13, 2026-11-19, a 16 GB M-series mac

A shared laptop: a datalib sync and two other bazel servers ran during
the run, so treat one-run differences under about half as noise.

```
250000 documents, 930597 rows of ~700 bytes; K = 5000 documents
median of 3 cold runs each, every run listed after it
by_row: built 875 MB in 17 s
  read                       2.57 s   [2.43, 2.57, 2.8]
  re-render, rolled back     3.14 s   [2.93, 3.14, 3.26]
  re-render, committed       5.78 s   file grew 96.2 MB
by_document: built 993 MB in 152 s
  read                       2.71 s   [2.31, 2.71, 2.9]
  re-render, rolled back     5.79 s   [5.67, 5.79, 7.74]
  re-render, committed      11.16 s   file grew 56.0 MB
```

What it says:

- **Keyed by document, the re-render writes 42% less** (56 MB against
  96 MB) — the clustering fact `rows_keyed_by_their_document_sit_together`
  holds at this size too.
- **But it ran about twice as slow**, with the unique index on `uuid`
  that the render's `ON CONFLICT (uuid)` upsert needs, and building the
  store in time order took 9× as long. Reading a document's rows was no
  faster: the time-ordered row key already keeps most of a thread's rows
  together.
- An earlier single cold run, before repeats, had the read 1.9× faster
  keyed by document (1.18 s against 2.22 s), and a split run of the delete
  alone 2.3× faster (1.37 s against 3.18 s). Neither held up as a median
  of three; they are why the script now repeats.

Not yet measured: the same key with no index on `uuid` and the upsert
conflicting on the key (`by_document_no_uuid_index` in the script), which
is what the render would do if it caught a row moved between documents
some other way.
