# `zoom` — render

One document per meeting UUID the raw store holds (a recording, a
report entry, or both), rendered through chat-common
(`datalib/backend/etl/chat-common/README.md`), so a meeting reads like a
conversation: who said what, in order, each section with its own
anchor and grid row.

## What a meeting's page holds

1. **Meeting** (`Zoom Meeting Details`, stamped at the meeting's
   start): the host, the length, the meeting ID, and who attended,
   each person once with every time they rejoined added up, as a chip
   where Zoom gave an email. Where Zoom would not say who came, the
   page says so and why. The recording's stored files (transcript,
   chat, timeline, captions, any video) are attachments here, linked
   from the blob store.
2. **Zoom AI Companion** (`Zoom Meeting Summary`, stamped at
   `summary_created_time`): Zoom's `summary_content`, which is markdown,
   as it came. A summary from before Zoom had that field is put
   together from `summary_overview`, `summary_details` and
   `next_steps`, escaped, since those are plain text.
3. **The transcript** (`Zoom Transcript`): the `TRANSCRIPT` file's cues,
   one section per turn, a turn being consecutive cues by one speaker.
   A cue's speaker is the text before its first `": "`; a cue without
   one is "Unknown speaker". A turn is stamped at the recording's
   `recording_start` plus the cue's offset, and its author is a chip
   when a participant of the same name gave an email. The words are
   escaped: they are what someone said.

The document's title is the meeting's topic, and its link is the
recording's `share_url`.

## Keys

| row | uuid minted from (`IdNamespace::Zoom`) |
|---|---|
| the document | `meeting`, the meeting UUID |
| details | `meeting_details`, the meeting UUID |
| summary | `meeting_summary`, the meeting UUID |
| a turn | `transcript_turn`, `(meeting UUID, file key, first cue's id)` |

The bucket is the meeting's document uuid, minted from the UUID alone,
so a meeting whose rows all went is declared empty and its page goes.
Every row a page reads (its row in `recordings`, `reported_meetings`,
`participants`, `summaries`, and its `recording_files` edges) is
declared, and the forward scan diffs those tables, so a run renders
only the meetings whose rows moved.

## Problems

- A transcript edge with no bytes (Zoom had lost it), or bytes the blob
  store no longer holds: `blob_missing` on the details section; the
  page renders without the transcript.
- A stamp that will not parse: `coercion_failed` on its section.

## Not rendered

The chat (`CHAT`), timeline and closed-caption (`CC`) files are linked,
not drawn; their layouts are not verified. A meeting with no recording has no transcript, only its
details and, where there is one, its summary.
