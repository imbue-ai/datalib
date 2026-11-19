# Should a render store's rows be keyed by their document?

The question: `grid_rows` is keyed by each row's own `uuid`, time-ordered
(`docs/dev/entity_ids.md` § "The layout"), with an index on
`markdown_uuid`. Every re-render of a document deletes and upserts its
rows by `markdown_uuid`. A profile of a Slack re-render of 62k documents
in a 9.8 GB render store spent most of the writer's time reading pages
from disk, in seeks from an index to a row. Keying rows by
`(markdown_uuid, uuid)` would put a document's rows side by side.

`scripts/doltlite_key_by_document.py` builds the same rows under three
layouts against the Bazel-built doltlite CLI and times, with the file's
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

All three stores are built first, then each of the three repeats takes
every layout in turn, so whatever else the machine was doing (a datalib
render was running throughout) lands on all of them alike.

```
250000 documents, 930597 rows of ~700 bytes; K = 5000 documents
by_row: built 875 MB in 20 s
by_document: built 993 MB in 137 s
by_document_no_uuid_index: built 836 MB in 79 s
median of 3 cold runs, interleaved across layouts; each run listed
by_row
  read                       2.78 s   [2.78, 2.64, 2.86]
  re-render, rolled back     3.20 s   [3.12, 3.2, 3.66]
by_document
  read                       1.11 s   [1.11, 1.09, 1.19]
  re-render, rolled back     2.77 s   [2.77, 2.45, 3.44]
by_document_no_uuid_index
  read                       1.11 s   [1.07, 1.11, 1.32]
  re-render, rolled back     1.66 s   [1.66, 1.44, 1.96]
by_row: re-render, committed   5.84 s   file grew 96.2 MB
by_document: re-render, committed   3.90 s   file grew 56.0 MB
by_document_no_uuid_index: re-render, committed   2.25 s   file grew 54.6 MB
```

What it says:

- **Keyed by document, reading a document's rows is 2.5× faster** (1.11 s
  against 2.78 s) and **a re-render writes 42% less** (55–56 MB against
  96 MB). The clustering fact `rows_keyed_by_their_document_sit_together`
  holds at this size.
- **The re-render is faster too, most of all without the unique index on
  `uuid`**: committed, 2.25 s against 5.84 s keyed by row, and 3.90 s
  with the index. The index is what the render's `ON CONFLICT (uuid)`
  upsert conflicts on; without it the upsert conflicts on the key, and a
  row that moved between documents has to be caught some other way.
- **Building a store in time order is slower keyed by document** (79 s
  and 137 s against 20 s): the rows arrive in time order and land all
  over a tree sorted by document. A render writes a page of documents at
  a time, so this is the cost of a full re-render's first write, not of
  an incremental one.

## Runs that did not hold up

Two earlier runs timed each layout's repeats one after another rather
than interleaved, while a sync and other bazel servers came and went. In
the first, keyed by document with the `uuid` index re-rendered in 5.79 s
against 3.14 s keyed by row; in the second, the same in 19.07 s against
5.32 s, with spreads of 3 s to 24 s. Whichever layout ran while the
machine was busiest looked slowest; interleaving the repeats is the fix.
