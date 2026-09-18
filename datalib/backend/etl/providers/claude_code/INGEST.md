# Claude Code: ingest

The `claude_code` source mirrors the transcripts Claude Code keeps on
this machine — every session run in the terminal, the desktop app or
an IDE extension — into a raw store, and renders each one as a document.
It has one ingest method today:

| method table               | reads                                          | needs credentials |
|----------------------------|------------------------------------------------|-------------------|
| `[steps.params.sessions]`  | `~/.claude/projects`, or its `path`            | no                |

A `cloud` table for sessions run on claude.ai is the planned second
method; `docs/dev/plans/agent_sessions.md` has what is known about that
route. It will share this store and this renderer, the way `claude`'s
`api` and `export` do, because `claude --teleport` writes a cloud
session's records into this same local layout.

## What is on disk

```
~/.claude/projects/
  <cwd with / and . replaced by ->/          one directory per working directory
    <sessionId>.jsonl                        the transcript, one JSON record per line
    <sessionId>/subagents/agent-<id>.jsonl   subagent transcripts (the Agent tool)
    <sessionId>/tool-results/<id>.txt        oversized tool results spilled to disk
    sessions-index.json                      sometimes present; not read
```

A transcript is append-only: Claude Code adds a line per event and
never rewrites one. Every content-bearing line — `user`, `assistant`,
`system` — carries a `uuid`, a `parentUuid`, a `sessionId`, a
`timestamp`, the `cwd` and `gitBranch` at the time, and the Claude Code
`version` that wrote it. A `user` record is either something the person
typed or a tool result (a `tool_result` block, with the structured
result beside it as `toolUseResult`), and either may be marked `isMeta`
when the harness injected it. An `assistant` record's `message.content`
is the API's block list: `text`, `thinking`, `tool_use`.

The other line types describe the session rather than carry content:
`custom-title` and `ai-title` (the name, rewritten as it changes),
`bridge-session` (the cloud session this one is linked to, and the
owning org and account), `pr-link`, `agent-name`, `last-prompt`,
`attachment` (the harness's own context injections: token reminders,
skill listings, file snapshots), `file-history-snapshot`, and a dozen
smaller ones.

A subagent's transcript carries the **parent's** `sessionId` on every
record plus its own `agentId`, and `isSidechain: true`.

## What the store holds

Two tables, both `WirePayloadRow`, neither with a bookkeeping sidecar
(a live session's file is re-read whole on every sync, and stamping
thousands of unchanged rows each time would churn the store for
nothing — the same call `airvisual` and `fsindex` made):

- **`transcripts`** — one row per file. The id is the session id, or
  `<session_id>#<agent_id>` for a subagent. Promoted columns:
  `session_id`, `agent_id`, `cwd`, `git_branch`, `title`, `started_at`,
  `updated_at`, `rel_path`. The payload is what the file says about
  itself: the title and where it came from (`custom`, `ai`, `agent`, or
  the first `prompt` when nobody named it), the first prompt, the
  versions and models seen, the cloud session id and org, the PR
  links, and a count of every line type.
- **`records`** — one row per content record, keyed by the `uuid`
  Claude Code gave it, with one promoted column, `transcript_id`: the
  writer composes it and the render's diff buckets on it. The payload
  is the line as written, and everything else a reader wants —
  `sessionId`, `type`, `timestamp`, `parentUuid`, `isSidechain` — is
  read off it (`payload->>'$.type'`); nothing in the tree queries
  those in SQL, so there is no index over them yet. Add an expression
  index over `payload->>'$.…'` the first time a query needs one, not a
  stored copy.

The bookkeeping lines fold into the transcript row and are not rows of
their own; `attachment` records are counted and dropped. A content
record with no `uuid` cannot be keyed and is counted as `unkeyed`. A
line that is not JSON is counted as `malformed` and stepped over — a
transcript is only ever appended to, so a torn last line is the
ordinary case for a session that is open right now.

## Incrementality

`fsscan` over the root for `*.jsonl`, with `file_checkpoint` holding
each file's hash under the `claude_code/sessions` scope. A file whose
hash moved is re-read whole and every row upserted; a row whose payload
did not change is a no-op to doltlite's content-addressed storage, so
`dolt_diff_records` names only the lines that are actually new, and
render re-draws only the transcripts they belong to. A file the host
cache can vouch for costs a `stat`.

**A transcript that vanishes keeps its rows.** Claude Code deletes old
sessions on its own schedule (`cleanupPeriodDays`), and outliving that
is half the point of a mirror. `--reset-and-redownload` is the way to
drop them.

## Render

One document per transcript, through chat-common. A person's prompt is
a `User Input` item; the assistant's text an `LLM Response` (authored
by the model name); a non-empty `thinking` block an `LLM Thinking`
aside; each `tool_use` block a `Tool Call` aside and each `tool_result`
block a `Tool Result` aside, so chat-common folds every run of tool
traffic into one collapsed block. An `isMeta` user record is a
`Harness Message` aside. A `system` record is a `System` item only when
it carries hook output or errors; the usual empty stop-hook summary
adds nothing. An assistant record whose only blocks are tool calls adds
no empty response.

A tool result's text is cut at the render step's
`max_tool_result_bytes` (default 16 KiB) with a marker saying how much
was cut; the raw store keeps all of it, and raising the limit and
re-rendering backfills. The same cut applies to a tool call's input.
A tool result names only the `tool_use_id` it answers; the tool's name
comes from the matching call in the same transcript.

A subagent's transcript is its own document, titled
`<its title> — subagent of <parent title>`. Nothing links the two
documents yet; an `edges` row from the parent's `Agent` call to the
subagent document is the obvious next step.

The grid's `project` column is the last component of the session's
`cwd` — what a person would call the project. A session bridged to a
cloud session gets `https://claude.ai/code/<cse_id>` as its link.

## Identity

Every key is one Claude Code minted and mints unique across machines,
so ids are `Scope::ProviderGlobal` under `IdNamespace::ClaudeCode`:
`session` on the session id, `agent_transcript` on
`<session_id>#<agent_id>`, `record` on the record uuid, and
`tool_use` / `tool_result` / `thinking_block` on
`<record_uuid>#<tool_use_id or block index>` — the same recipe as
`claude_render/src/render/ids.rs`.

## Secrets

Transcripts contain whatever the tools saw: the output of `env`, a
pasted token, the contents of a `.env` file. The raw store keeps the
record as Claude Code wrote it; the rendered document and
`grid_rows.text` carry the same bytes, cut at the limit above. Nothing
here redacts. Treat a mirror of this source as sensitive as the
sessions themselves.

## Sample data

`tests/fixtures/claude_code_tng/` is a `~/.claude/projects` tree with
two TNG-themed sessions and one subagent transcript, generated by
`tests/fixtures/make_claude_code_fixtures.py` from record shapes copied
off Claude Code 2.1.270. `tests/fixture_e2e.rs` ingests and renders
it; the central pipeline at `//tests/fixtures:ingested_tng` runs it
beside every other source.
