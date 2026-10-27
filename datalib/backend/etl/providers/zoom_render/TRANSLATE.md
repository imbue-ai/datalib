# `zoom` — render

One document per meeting UUID the raw store holds (a recording, a
report entry, or both), rendered through chat-common
(`datalib/backend/etl/chat-common/README.md`), each section with its
own anchor and grid row.

## What a meeting's page holds

1. **Meeting** (`Zoom Meeting Details`, stamped at the meeting's
   start): the host, the length, the meeting ID, the kinds of file Zoom
   keeps of its recording (`audio transcript`, `shared screen with
   speaker view`, …, each once), and who attended, each person once
   with every time they rejoined added up, as a chip where Zoom gave an
   email. Where Zoom would not say who came, the page says so and why.
2. **Zoom AI Companion** (`Zoom Meeting Summary`, stamped at
   `summary_created_time`): Zoom's `summary_content`, which is markdown,
   as it came. A summary from before Zoom had that field is put
   together from `summary_overview`, `summary_details` and
   `next_steps`, escaped, since those are plain text.

The document's title is the meeting's topic, and its link is the
recording's `share_url`.

No transcript is on the page and no recording file is linked: the
download fetches none (INGEST.md, "Transcripts and recording files are
not downloaded").

## Keys

| row | uuid minted from (`IdNamespace::Zoom`) |
|---|---|
| the document | `meeting`, the meeting UUID |
| details | `meeting_details`, the meeting UUID |
| summary | `meeting_summary`, the meeting UUID |

The bucket is the meeting's document uuid, minted from the UUID alone,
so a meeting whose rows all went is declared empty and its page goes.
Every row a page reads (its row in `recordings`, `reported_meetings`,
`participants` and `summaries`, all keyed by the UUID) is declared, and
the forward scan diffs those tables, so a run renders only the meetings
whose rows moved.

## Problems

A stamp that will not parse: `coercion_failed` on its section.
