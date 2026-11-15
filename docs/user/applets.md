# The applets

An **applet** is a long-lived server the app runs: a part of the app
with its own HTTP routes, and for some of them a card in the app. Each
is one `[[applets]]` entry in the config. `datalib-http` starts every
entry when it starts and keeps it running, so an applet's routes answer
only while the app is up. A sync needs no applet; it is the steps that
write the data, and the applets read it.

This page says what the applets datalib ships serve, for an agent
calling them or a person deciding which to keep. To write your own, see
[writing an applet](../dev/applets.md).

## Calling one

Every applet is served at `/applet/<id>/` on `datalib-http`, behind the
API token. `<id>` is the entry's `id` in the config, and the routes
below are relative to it:

```sh
TOKEN=$(cat <data_root>/system/api-token)
curl -H "Authorization: Bearer $TOKEN" "<origin>/applet/unified_document_index/search?q=warp"
```

A running server writes its address on start (`--url-file` writes it to
a file). The token is minted fresh each start, so read the file again
rather than keeping the value. An answer of 502 with `no applet "<id>"`
means the config has no such entry; add its `[[applets]]` table below
and the app starts it within a moment, with no restart.

## What ships

| applet (`datalib-applet <name>`) | serves | a card in the app |
|---|---|---|
| `unified_document_index` | search, the grid, the documents, what each source says about a handle | the grid and document view, built into the app |
| `datalib_contacts` | your own contacts: which handles are one person, labels on people and senders | the person card and the contacts card, built into the app |
| `unified_time_series_index` | the time series, and their points over a range | the chart, built into the app |
| `photos` | photos and videos | a contact sheet |
| `slack` | one Slack mirror's channels and threads | a channel browser |

`datalib-dag init` writes the first two. `datalib-step examples <type>
--append` adds the time-series index when a source that has time series
is the first.

In the app, **Add data source** lists the contacts app and the photos
applet under "Apps that read every source", each with a button that
turns it on or off; the Lightroom form offers the photos applet too,
and editing a Lightroom source you already have is enough to turn it
on. Turning one off takes its entry out of the config; anything it
stored stays on disk. From the command line, add the entry yourself:

```toml fragment
[[applets]]
group = "unified_document_index"
id = "datalib_contacts"
command = "datalib-applet datalib_contacts"

[[applets]]
id = "photos"
command = "datalib-applet photos"

[[applets]]
group = "slack"
id = "slack_view"
command = "datalib-applet slack"
[applets.params]
tree = "slack/render_markdown"
workspace = "Slack"
```

`group`, which is optional, is the source or index its row on the
Manage screen is filed under.

## `unified_document_index`

The one applet the app cannot run without. What it answers is mostly in
other pages: [the query language](searching.md) is what `q=` takes, and
the agent guide's [Reading the mirrored data](../agent_user.md#reading-the-mirrored-data)
has the parameters.

| route | does |
|---|---|
| `GET /search?q=` | a page of matching rows, as `datalib-applet search` prints from the command line |
| `GET /search/groups?q=&by=` | how many rows each group of a column holds |
| `GET /search/keys`, `GET /search/values?key=&typed=&q=` | every key the search takes, and the values one takes |
| `GET /docs` | the documents |
| `GET /chat/{markdown_uuid}` | one document: its markdown as `body`, where each message in it begins as `sections`, and what the index knows of it |
| `POST /people` | `{"handles": […]}`: every source's record of each handle, best first |
| `GET /problems?q=` | what a step could not fully do to a record; each row says whether a person muted it (`muted`), and `muted:no` leaves the muted ones out |
| `GET /problems/groups`, `/problems/keys`, `/problems/values` | the same search over problems as the rows' above |
| `POST /problems/mute` | `{"problem_uuids": […], "muted": true}` mutes those rows, `false` unmutes them; add `"agent": "<name>"` when an agent mutes for a person. A muted row stays listed and counts toward nothing until it changes |
| `GET /embedding_map` | the documents' embeddings laid out on a plane; `/embedding_map/matches` the ones a search finds |
| `POST /sql` | one `SELECT` over any store under the data root, a page of rows at a time (below) |
| `GET /sql/stores` | the stores `/sql` reads |

How a document names people, times and attachments, for an app that
shows one: [showing a rendered document](rendered_documents.md).

### Reading any store with SQL

`POST /sql` runs one `SELECT` you write against any store under the data
root and answers a page of rows as JSON. It is the shell's
`datalib-doltlite -readonly` over HTTP, behind the API token like every
route here: nothing to install, and the same answer from a container or
another machine that can reach the app. What each store's tables hold is
in [reading the raw stores](raw_stores.md), [the grid index's
columns](../dev/grid_rows.md) and [the log store](../dev/logging.md).

Every example below sends the token and posts JSON, so set them up once:

```sh
TOKEN=$(cat <data_root>/system/api-token)
SQL="<origin>/applet/unified_document_index/sql"
ask() { curl -s -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$1" "$SQL"; }
```

**1. Find the store.** `GET /sql/stores` lists every store it reads, by
the name you send back as `store`:

```sh
curl -s -H "Authorization: Bearer $TOKEN" "$SQL/stores"
```

```json
{"stores": [{"store": "slack/ingest/entities.doltlite_db", "kind": "doltlite", "bytes": 52183040},
            {"store": "system/runs/runs.sqlite", "kind": "sqlite", "bytes": 8650752}, …]}
```

**2. See its tables and columns.** Both are ordinary `SELECT`s:

```sh
ask '{"store": "system/runs/runs.sqlite",
      "sql": "SELECT name FROM sqlite_schema WHERE type = :type ORDER BY name",
      "params": {"type": "table"}}'
ask '{"store": "system/runs/runs.sqlite",
      "sql": "SELECT name, type FROM pragma_table_info(:table)",
      "params": {"table": "log"}}'
```

**3. Ask it something.** Put each value in `params` rather than in the
SQL, by name or by position:

```sh
ask '{"store": "system/runs/runs.sqlite",
      "sql": "SELECT seq, step, msg FROM log WHERE level = :level AND step LIKE :step ORDER BY seq DESC",
      "params": {"level": "error", "step": "slack/%"},
      "limit": 50}'
```

```json
{"store": "system/runs/runs.sqlite", "kind": "sqlite", "commit": null,
 "columns": ["seq", "step", "msg"],
 "rows": [[90211, "slack/ingest", "…"], …],
 "next_offset": 50}
```

The request:

- **`store`** is the file's path under the data root: a `.doltlite_db`
  or `.sqlite` file in a step's tree (`<group>/<step>/…`), or one of
  `system/runs/runs.sqlite` (the log and every run's metrics),
  `system/supervisor.sqlite` (sync requests and each step's state),
  `system/feedback.doltlite_db`, `system/remote_media.doltlite_db`,
  `system/disk_stats.sqlite` and `system/usage.doltlite_db`. Nothing
  else under `system/` is a store here. `GET /sql/stores` lists them,
  with each one's `kind` and `bytes`, the search index's vectors
  (`unified_document_index/search_index/inference_cache.sqlite`) among them.
  Any other store deeper than `<group>/<step>/<file>` reads by name but
  is not listed.
- **`sql`** is one statement that returns rows: a `SELECT`, a `WITH …
  SELECT` or a `VALUES`. A trailing `;` is fine; a second statement is
  refused. `PRAGMA table_info(t)` is written `SELECT * FROM
  pragma_table_info('t')`.
- **`params`** gives the statement's parameters their values, in one of
  two shapes:
  - **An object, for named parameters.** `:name`, `@name` and `$name`
    are all named; key each by its name, with or without the sigil
    (`{"level": "error"}` or `{":level": "error"}`). A name used twice
    in the statement takes one value.
  - **A list, for numbered ones.** `?` takes the next value in order,
    and `?2` the second wherever it appears.

  Don't mix the two in one statement. Every parameter needs a value and
  every value a parameter, so a misspelt name is an error rather than a
  `NULL`. A value is a string, a number, `true`/`false` (bound as 1/0),
  `null`, or `{"base64": "…"}` for a blob. To match a set, send one JSON
  array as a string and read it with `json_each`: `"sql": "… WHERE id IN
  (SELECT value FROM json_each(:ids))", "params": {"ids": "[3, 4]"}`.
- **`limit`** is the page's rows, 500 by default and at most 10,000;
  **`offset`** is where the page starts.
- **`commit`**, for a doltlite store, is the commit to read. Leave it out
  and the page reads the store's newest commit and says which.

The answer's `rows` are lists in `columns`' order. An integer is a JSON
number, exact to 64 bits (JavaScript's `JSON.parse` rounds past 2^53;
select it as text if you read it there), a real is a number, text is a
string, and a blob is `{"base64": "…"}`.

**Paging.** Send the same request again with `offset` set to
`next_offset`, and stop when it is `null`:

```sh
ask '{"store": "system/runs/runs.sqlite", "sql": "SELECT seq, msg FROM log ORDER BY seq",
      "limit": 1000, "offset": 1000}'
```

 Give the statement an
`ORDER BY`, or two pages may disagree on the order. On a doltlite store,
send the first page's `commit` with every later page, and they all read
that commit however far the sync has moved on. A plain SQLite store (the
grid index, the run store) has no commits, so a page reads it as it is
then; to walk one that is being written, page by key instead: `WHERE
seq > :after ORDER BY seq` with `{"after": <the last seq you read>}`. A page can stop short of `limit` when its
values pass 8 MB; `next_offset` still says where to go on.

**What it will not do.** It reads, and nothing else. The store is opened
read-only and pinned to one commit, and every statement is checked before
it runs: reading tables and calling SQLite's functions is allowed, and so
are doltlite's that only answer (`dolt_hashof`, `dolt_merge_base`,
`dolt_version`), its history tables (`dolt_log()`, `dolt_diff_<table>`,
`dolt_history_<table>`) and the `pragma_…` tables that describe a schema.
Anything else is refused with a 403 naming it: a write, `ATTACH`, and
doltlite's functions that change or reach past the store (`dolt_commit`,
`dolt_push`, `dolt_clone`, `dolt_creds_new`). A query still running after
20 seconds is stopped with a 504; narrow it or page with a smaller
`limit`. Four run at once, and the rest wait their turn.

Other errors are a JSON `{"error": "…"}`: 400 for a statement SQLite
cannot run (its message, such as `no such table`) or a request that does
not fit the above, and 404 for a store or commit that is not there.

## `datalib_contacts`

Links handles to contacts, and the proposals and review around that.
[The contacts app](contacts_app.md) is its own page. It lists its routes
itself, at `GET /applet/datalib_contacts/` and in `datalib-applet
datalib_contacts --help`.

## `unified_time_series_index`

| route | does |
|---|---|
| `GET /time_series` | the time series (`?source_id=`, `?q=`) |
| `GET /time_series/{id}` | one |
| `POST /points` | `{"ids": […], "from": …, "to": …, "budget": n}`: each one's points in the range or, past `budget` points, that many buckets of min, max and mean |

## `photos`

| route | does |
|---|---|
| `GET /photos` | a page of photos and videos, newest taken first |
| `GET /photo/{uuid}` | one, by the uuid of its row in the grid |
| `GET /photo/{uuid}/image?size=thumb` or `size=full` | its picture, from the source's own previews, else its original |
| `GET /photo/{uuid}/original` | the file, in byte ranges |

## `slack`

One entry serves one Slack source: its `params` name the render store
(`tree`) and what to call it (`workspace`).

| route | does |
|---|---|
| `GET /channels` | every channel, with thread and message counts |
| `GET /channel?name=` | one channel's threads, each by its first message |

## Every applet

`GET /health` answers `{"ok": true}` on each of them.
