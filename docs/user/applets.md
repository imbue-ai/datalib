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
is the first. The other two are yours to add:

```toml fragment
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
| `GET /chat/{markdown_uuid}` | one document: its markdown as `body`, and what the index knows of it |
| `POST /people` | `{"handles": […]}`: every source's record of each handle, best first |

How a document names people, times and attachments, for an app that
shows one: [showing a rendered document](rendered_documents.md).
| `GET /problems?q=` | what a step could not fully do to a record |
| `GET /problems/groups`, `/problems/keys`, `/problems/values` | the same search over problems as the rows' above |
| `GET /embedding_map` | the documents' embeddings laid out on a plane; `/embedding_map/matches` the ones a search finds |

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
