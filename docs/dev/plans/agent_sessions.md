# Agent sessions: Claude Code, Cowork, Codex, Gemini CLI

*Investigation (2026-09-16); the `claude_code` source's local method is
built (same day — `datalib/backend/etl/providers/claude_code/INGEST.md`
is the reference) and the `codex` source's local method is built
(2026-09-22 — `datalib/backend/etl/providers/codex/INGEST.md`); the
rest is not.* What it would take to
mirror the transcripts of the coding and desktop agents — Claude Code
(local and cloud), Claude Cowork, OpenAI's Codex (local and cloud), the
ChatGPT bulk export the `chatgpt` source still lacks, and Google's
Gemini CLI — into datalib. Everything marked *measured* was checked on
one mac on that date; everything marked *from source* was read out of
the vendor's own client rather than guessed; the rest is marked as
unverified.

## The short version

| source | method | reach | how it is read | auth | status of the route |
|---|---|---|---|---|---|
| Claude Code | `local` | files | `~/.claude/projects/<cwd-slug>/<session>.jsonl` | none | *measured*: 282 sessions, 630 MB on this machine |
| Claude Code | `cloud` | origin | `api.anthropic.com/v1/code/sessions` + `…/{id}/teleport-events` | Claude Code's own OAuth token (keychain) | *from source* (the `claude` binary); undocumented; **not yet probed** |
| Cowork (desktop) | `local` | files | `…/Application Support/Claude/local-agent-mode-sessions/…/*.jsonl` | none | documented by Anthropic; none on this machine to measure |
| Cowork (web/mobile) | `cloud` | origin | unknown — `cse_` ids, same id space as Claude Code cloud | claude.ai session or OAuth | **open question**, needs a browser probe |
| Any of the above, Enterprise only | — | origin | Compliance API `/v1/compliance/apps/sessions/{local,remote}` | Compliance Access Key | documented and stable, but Enterprise plans only |
| ChatGPT | `export` | files | `conversations.json` from the data-export zip | none | shape already parsed by the `api` method's renderer |
| Codex CLI | `local` | files | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` + `state_5.sqlite` | none | **built** (rollouts, both format generations; the SQLite is not read yet) |
| Codex cloud | `cloud` | origin | `chatgpt.com/backend-api/wham/tasks/list`, `…/wham/tasks/{id}` | the ChatGPT bearer the `chatgpt` source already holds | *from source* (`openai/codex`); undocumented |
| Gemini web | takeout | files | `My Activity/Gemini Apps/MyActivity.html` | none | **already built** (`google_takeout`) |
| Gemini CLI | `local` | files | `~/.gemini/tmp/<project-hash>/chats/session-*.json` | none | *measured*: 22 sessions here |
| Gemini web | `api` | origin | cookie-authenticated `batchexecute` RPCs | `__Secure-1PSID` cookies | reverse-engineered, cookies now expire in hours; not recommended |

Recommended order: Claude Code `local` first (largest corpus, no
credentials, and its parser is the one every other Claude surface
reuses), then the ChatGPT `export` method (an afternoon), then Codex
`local` + `cloud`, then Claude Code `cloud` once the probe below is
done, then Gemini CLI. Cowork `local` is nearly free after Claude Code
`local`; Cowork `cloud` waits on the same probe.

## What every one of these has in common

A session transcript is not a chat conversation, and the existing
`claude` / `chatgpt` sources are the wrong shape for it in three ways.

**One row per record, not per session.** A claude.ai conversation is
one JSON document with its messages nested inside; the raw store holds
one row per conversation and re-fetches the whole thing when
`updated_at` moves. A session is an append-only log — one JSON record
per line, thousands of lines, most of them tool calls and tool results
— that grows while the session is open and is never edited. Storing it
as one row per *record*, keyed by the record's own `uuid` with the
session id as a column, is what makes both incrementality and streaming
work: a re-scan of a still-open session upserts only the lines it has
not seen, and the render step can draw a session that is still running.

**Tool results are the bytes.** In the largest session here, tool
results (`user` records whose content is a `tool_result` block) are
about 40% of the file, and `attachment` records (the harness's own
context injections: token reminders, skill listings, file snapshots)
another 10%. A tool result that echoes a 2 MB file is not unusual.
Two consequences: the render step must fold these rather than print
them (chat-common's `<details>` for a run of tool steps already does
this), and it is worth deciding up front whether a tool-result body
over some threshold goes to the blob CAS rather than the JSONB
payload. `docs/dev/plans/multimodal_retrieval.md` §4 already counts the
same text stored five times; this source would make that worse.

**Transcripts are full of secrets.** A tool result can contain the
output of `env`, a pasted API key, the contents of `.env`. The
Compliance API strips nothing either and says so. Datalib's rule is to
store the record as the source wrote it, so nothing here changes that —
but the render step should not put those bytes into `grid_rows.text`
verbatim, and the source needs a sentence in its INGEST.md saying so.
Read `docs/dev/plans/data_lib_as_a_library/data_handling_practices.md`
before designing the row.

**Identity is mostly easy.** Every one of these tools mints a UUID per
session (Claude Code and Cowork a v4, Codex a v7, Gemini CLI a v4 plus
a per-project hash), so `grid_rows.uuid` follows the
`docs/dev/entity_ids.md` rule, with no account — none of these tools
names one on a record — and no hashing of our own. Per record, only Claude Code mints a UUID; a Codex
rollout line carries `timestamp`, `type` and `payload` and nothing
else (measured, 0.104–0.115; a newer Codex adds an `ordinal`), so its
key is the line's number within the thread, which the append-only
file keeps stable.

**Sessions have parents.** Claude Code writes a subagent's transcript
to `<session>/subagents/agent-<id>.jsonl` (72 of them here), and a
record carries `isSidechain` and `parentUuid`. Codex's `session_meta`
has `forked_from`. A subagent transcript is its own document with an
edge to the parent, not a section of it.

## Claude Code

### `local` — *measured*

Layout under `~/.claude/projects/`:

```
<cwd with / and . replaced by ->/          one directory per working directory
  <sessionId>.jsonl                        the transcript, one JSON record per line
  <sessionId>/subagents/agent-<id>.jsonl   subagent transcripts (Agent tool)
  <sessionId>/tool-results/<id>.txt        oversized tool results spilled to disk
  sessions-index.json                      sometimes present; not load-bearing
```

Plus `~/.claude/history.jsonl`, one line per prompt typed
(`{display, timestamp, project, sessionId}`), which is a cheap index of
which sessions exist and where they ran.

The records. Every content-bearing record carries `uuid`,
`parentUuid`, `sessionId`, `timestamp`, `cwd`, `gitBranch`, `version`
(the Claude Code version that wrote it), `isSidechain` and `type`. The
`type` vocabulary on this machine, over all 282 sessions
(versions 2.1.163–2.1.270):

| type | count | what it is |
|---|---|---|
| `assistant` | 68 000 | an API message: `message.content[]` of `text` / `thinking` / `tool_use`, plus `message.model`, `requestId`, `effort` |
| `user` | 39 000 | a human turn (`message.content` is a string) **or** a tool result (`content[]` of `tool_result`, with `toolUseResult` beside it) |
| `attachment` | 40 000 | the harness's own context injections; `attachment.type` is one of ~20 (`total_tokens_reminder` dominates) |
| `system` | 1 700 | hook output, stop reasons |
| `custom-title`, `ai-title` | 9 000 | the session's title, rewritten as it changes; **take the last one** |
| `bridge-session` | 7 900 | links this local session to a cloud session id (`cse_…`) and the owning org and account |
| `file-history-snapshot` / `-delta` | 560 | the file-history for undo; ignore |
| `last-prompt`, `atis-latch`, `queue-operation`, `pr-link`, `mode`, `agent-name`, … | | bookkeeping; `pr-link` is worth keeping as an edge |

Thinking blocks are present but usually empty with a `signature`
(redacted thinking), so there is little to render there.

The title lives in the transcript (last `custom-title` / `ai-title`
record). The desktop app keeps a second index —
`~/Library/Application Support/Claude/claude-code-sessions/<account>/<org>/local_<id>.json`,
210 of them here, with `title`, `cliSessionId`, `cwd`, `model`,
`createdAt`, `lastActivityAt`, `completedTurns` — which is the only
place the *desktop's* title and archive state live. Read it when it
exists; do not require it.

Incrementality is `fswalk` plus a per-file byte offset: a session file
only ever grows, so the cursor is "bytes consumed per path" and a
re-scan reads the tail. `datalib_etl::fswalk` gives the changed-file
detection; the offset is new. A file that *shrinks* (the user deleted
and re-created the session id, which does not happen in practice) is a
full re-read.

Teleported sessions land here too: `claude --teleport` writes the
cloud session's history into `~/.claude/projects/` under a new local
session id, with a `bridge-session` record naming the cloud id. So a
user who teleports already gets those sessions through `local`, and
the `cloud` method below has to dedupe against them via that record.

### `cloud` — *from source*, not yet probed

Cloud sessions (claude.ai/code, the desktop's "Cloud" option, the
mobile Code tab, `claude --cloud`, routines) do not touch
`~/.claude/projects/` unless teleported. The endpoints are in the
`claude` binary (2.1.251, read with `strings`):

```
GET https://api.anthropic.com/v1/code/sessions?limit=100&cursor=…
    → { data: [ { id, title, status, worker_status,
                  config.sources[{type:"git_repository", url, revision}], … } ] }
GET https://api.anthropic.com/v1/code/sessions/{id}
GET https://api.anthropic.com/v1/code/sessions/{id}/teleport-events?limit=1000&cursor=…
    → { data: [ …records with uuid… ] }      what --teleport writes as the local transcript
GET https://api.anthropic.com/v1/code/sessions/{id}/events?limit=N&sort_order=desc
```

Auth is what the binary calls `teleport-org`: `Authorization: Bearer
<accessToken>` plus `x-organization-uuid: <org>`, where the token is
`claudeAiOauth.accessToken` in the macOS keychain item
`Claude Code-credentials` (refreshed through
`/v1/code/auth/refresh`) and the org is
`~/.claude.json` → `oauthAccount.organizationUuid`. An optional
`X-Trusted-Device-Token` header rides along when the device is bound.

This is a materially better route than the `claude` source's: it is
`api.anthropic.com`, not claude.ai, so **Cloudflare's fingerprint wall
is not in the path** and neither latchkey nor `curl-impersonate` is
needed. The cost is that it is undocumented — Simon Willison's
`claude-code-transcripts web` used the previous generation of these
endpoints (`/v1/sessions`, `/v1/session_ingress/session/{id}`) and
broke in February 2026 when they moved
(github.com/simonw/claude-code-transcripts#77). Expect the same again.

The teleport-events records are, by construction, the local JSONL
record shape — the CLI writes them straight into a local transcript —
so `local` and `cloud` share one parser and one raw store, exactly as
the `claude` source's `api` and `export` do. That is the argument for
making `cloud` a second method table on a `claude_code` type rather
than a type of its own.

**Not verified in this session**: reading the keychain token was
declined by the tool sandbox (correctly), so nothing above has been
exercised. The probe is two `curl`s with that token and org header:
`/v1/code/sessions` (does it list cloud Code sessions? Cowork
sessions? archived ones?) and one session's `teleport-events` (is the
record shape really the local one, and is the page cursor stable?).

### Enterprise: the Compliance API

For a Claude Enterprise org there is a documented, stable route:
`GET /v1/compliance/apps/sessions/local` (Claude Code, Cowork on the
desktop, `clls_` ids) and `…/sessions/remote` (Cowork on the web,
`cse_` ids), each with a `/{id}/messages` transcript endpoint,
authenticated with a Compliance Access Key. It is read-only, paginated,
retained six years, and strips thinking and system prompts and
truncates tool blocks at 10 KB unless asked for more. It explicitly
does **not** return Claude Code on the web. It is the right thing for
an admin backing up an org and the wrong thing for a person backing
up themselves; it is listed here so nobody rediscovers it.

## Cowork

**Desktop Cowork is local.** Anthropic's own data-storage page
documents `~/Library/Application Support/Claude/local-agent-mode-sessions/<account>/<org>/`
holding, per session, a `local_<id>.json` state file, a working
directory with `uploads/` and `outputs/`, an HMAC-chained
`audit.jsonl`, and the transcript itself in the same JSONL format as
Claude Code (`<session>/.claude/projects/-sessions-<process>/<cliSessionId>.jsonl`
by one third-party measurement). This machine has never run a local
Cowork session — the directory holds only skill scaffolding — so none
of that is measured here. The directory is mid-rename to
`claude-code-sessions/` upstream and both names can coexist. Once
Claude Code `local` exists, Cowork `local` is the same parser over a
second root plus the `local_*.json` metadata.

**Web and mobile Cowork is cloud-only and its consumer route is
unknown.** Its sessions carry `cse_` ids — the same prefix as Claude
Code cloud sessions, which suggests one backing service — and the
Compliance API is the only documented reader. Whether
`/v1/code/sessions` lists them, or claude.ai's frontend calls a
different endpoint, is the open question; a logged-in browser's
network log on claude.ai answers it in a minute. (This session's
Chrome extension was not connected, and claude.ai's API was returning
Cloudflare challenges to the CLI path.)

The consumer data export (Settings → Privacy) is documented as
"conversation data and user data"; there is no evidence it includes
Code or Cowork sessions.

## OpenAI

### ChatGPT `export` — the cheap one

The `chatgpt` type has only an `api` method. The data-export zip
(Settings → Data controls → Export) is `conversations.json` (or
`conversations-000.json, …` when large), plus `user.json`,
`message_feedback.json`, `shared_conversations.json`, `chat.html`, and
the attachment files. Each conversation is the same `mapping` tree
(parent/child nodes, one per edit or regenerate) the `api` method gets
from `/backend-api/conversation/{id}`, so the renderer needs nothing
new; the ingest is a file walk into the same raw store. Copy the
`claude` source's `export` pattern including its prune-to-snapshot
rule and its one-way-door hazard (INGEST.md there, §"The hazard").
Attachment files in the zip go to the CAS.

### Codex CLI `local` — *measured*

```
~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuidv7>.jsonl
~/.codex/history.jsonl                {session_id, ts, text} per prompt
~/.codex/state_5.sqlite               threads(id, rollout_path, created_at, updated_at,
                                      cwd, title, git_branch, git_sha, git_origin_url,
                                      tokens_used, archived, first_user_message, …)
```

Record `type` is one of `session_meta` (id, timestamp, cwd,
`cli_version`, `model_provider`, `base_instructions`, git info),
`turn_context`, `response_item` (`payload.type` = `message`,
`function_call`, `function_call_output`, `reasoning`, …) and
`event_msg` (`user_message`, `task_started`, `task_complete`, …).
**That last list is the 0.115 vocabulary.** By 0.155 the
`user_message` event is gone — `item_completed` replaced it — and a
message instead tags its own parts (`content_item_kinds`), which is
the better signal and the one the provider prefers; see the provider's
INGEST.md § "Two generations".
Plain SQLite, so titles come from `state_5.sqlite` via the ordinary
`sqlx` pool, not the mirror engine. Same append-only file, same
byte-offset cursor as Claude Code.

### Codex cloud — *from source*

`openai/codex`'s `codex-rs/backend-client/src/client.rs`:

```
GET https://chatgpt.com/backend-api/wham/tasks/list?limit=&cursor=&task_filter=&environment_id=
GET https://chatgpt.com/backend-api/wham/tasks/{id}                 CodeTaskDetailsResponse
GET https://chatgpt.com/backend-api/wham/tasks/{id}/turns/{turn}/sibling_turns
```

Auth is the ChatGPT bearer token plus a `chatgpt-account-id` header —
the same token the `chatgpt` source already keeps under latchkey's
`chatgpt` service, and the same Cloudflare front, so the existing
`curl-impersonate` dispatch applies unchanged. A task's detail
response carries its turns, messages and diff; the exact shape is in
`codex-rs/codex-backend-openapi-models`. Undocumented, with the usual
caveat.

Given the shared credential, `codex` as a type with `local` and
`cloud` method tables mirrors `claude_code`; a local Codex session and
a cloud task are different records (a rollout vs. a task with turns),
so unlike Claude Code the two methods here do **not** share a parser.

## Gemini

**Web history is already covered** by `google_takeout`'s
`gemini_apps` walker over `My Activity/Gemini Apps/MyActivity.html`.
Takeout can also emit that activity as JSON, which would be a smaller
parser than the HTML one; not urgent.

**Gemini CLI `local`** — *measured*:
`~/.gemini/tmp/<sha256 of project path>/chats/session-<ts>-<id>.json`,
one JSON document per session: `{sessionId, projectHash, startTime,
lastUpdated, messages[]}` with `messages[].type` ∈ `user`, `gemini`,
`info` and, on `gemini` turns, `model`, `thoughts`, `tokens`,
`toolCalls`. Not append-only JSONL — the whole file is rewritten — so
the cursor is the file hash, as for `google_takeout`. 22 sessions
here. The project hash is not reversible; the cwd has to come from
`logs.json` beside it or be left as the hash.

**Gemini web API** — the only live route is the reverse-engineered
one (`gemini-webapi`: `list_chats()`, `read_chat(id)` over the
`batchexecute` RPC, authenticated with `__Secure-1PSID` /
`__Secure-1PSIDTS` cookies). Chromium's device-bound session
credentials now expire those cookies within hours, and the library
refreshes them by polling. That is a worse position than Cloudflare:
not recommended while Takeout works. Google's developer Gemini API
has no access to a consumer's gemini.google.com history at all.

**Antigravity** (Google's IDE) keeps conversations at
`~/.gemini/antigravity/conversations/<uuid>.pb` — protobuf with no
published schema. Skip.

## Decided (2026-09-16): three types over one engine

Local ingestion for all three is being built, Claude Code first
(**built**), Codex second (**built**, 2026-09-22), Gemini CLI next. The
type question was settled against the `email` precedent
(`docs/dev/email_download_modes.md` §1–2): transports share a type only
when the same thing ingested two ways **dedupes** — `email_id` is the
`Message-ID` in every mode. A Claude Code session, a Codex session and a
Gemini CLI session are never the same thing, so they are three types;
Claude Code and `claude` (claude.ai chats) never overlap either, so
Claude Code is not a third table on `claude`. What the three share is
machinery, on the `sqlite_mirror` / `timeseries_render` precedent:

- `claude_code`, `codex`, `gemini_cli` — three ordinary thin provider
  sets, each with its own raw schema (we store what the tool wrote),
  `SourceType` and `Provider` variants, Manage-screen row and wizard
  entry. The local method is `[steps.params.sessions]` with a `path`
  that defaults to the tool's standard store, so the table can be
  empty the way `[steps.params.gmail]` is. A `cloud` table joins
  `claude_code` and `codex` later, sharing each type's store and
  renderer, exactly as `claude`'s `api` and `export` do.
- one shared ingest engine for JSONL session trees. Claude Code's
  ingest turned out to be `fsscan` + `file_checkpoint` + a parser
  (`ingest/mod.rs` is ~200 lines, and a changed file is simply
  re-read whole — doltlite's content addressing makes the unchanged
  rows free, so the byte-offset cursor was not needed). Codex's is the
  same ~200 lines with a different parser and two scan roots; the
  shape has now repeated and the engine is there to pull out, though
  what it would save is the `RawDb` boilerplate and the scan loop,
  not the parsers. Gemini CLI — whole-file JSON — uses the same hash
  cursor either way.
- one shared render crate over a normalized turn model: `tool_use` /
  `tool_result` runs folded into chat-common's `<details>`, one `h2`
  per human turn, subagent transcripts as separate documents joined by
  an edge.

Still open: the tool-result size above which a body goes to the CAS
rather than the payload; and Cowork desktop, which is the Claude Code
JSONL under a second root and could be a second `claude_code` group
today, but names a product a person recognizes — decide when there is a
local Cowork session to measure.

## Sources

- Claude Code cloud sessions and teleport: code.claude.com/docs/en/claude-code-on-the-web
- Compliance API sessions: platform.claude.com/docs/en/manage-claude/compliance-sessions
- Desktop / Cowork on-disk layout: claude.com/docs/third-party/claude-desktop/data-storage
- Cowork transcript measurement: brycewatson.com/blog/13-cowork-conversation-transcripts/
- Previous-generation cloud endpoints and their breakage: github.com/simonw/claude-code-transcripts, issue #77
- Codex cloud client: github.com/openai/codex, `codex-rs/backend-client/src/client.rs`
- Gemini web reverse engineering: github.com/HanaokaYuzu/Gemini-API
