# Codex: ingest

The `codex` source mirrors the rollouts Codex CLI keeps on this machine
— every thread run in the terminal, `codex exec`, or an IDE extension
— into a raw store, and renders each one as a document. It has one
ingest method today:

| method table               | reads                                      | needs credentials |
|----------------------------|--------------------------------------------|-------------------|
| `[steps.params.sessions]`  | `~/.codex`, or its `path`                  | no                |

A `cloud` table for tasks run on Codex cloud is the planned second
method; `docs/dev/plans/agent_sessions.md` has what is known about that
route. A cloud task is a different record from a local rollout — a task
with turns and a diff, not a line-per-event log — so it will share this
store's `transcripts` table and this renderer's document shape, and
have a parser of its own.

## What is on disk

The Codex home (`CODEX_HOME`, by default `~/.codex`) holds:

```
sessions/YYYY/MM/DD/rollout-<timestamp>-<thread id>.jsonl   one file per thread
archived_sessions/…                                        where an older Codex moved an archived thread
history.jsonl        {session_id, ts, text} per prompt typed; an index, not read
state_5.sqlite       threads(id, title, cwd, archived, …); plain SQLite, not read yet
```

The ingest walks `sessions/` and `archived_sessions/`, each under its
own file cursor (`codex/sessions`, `codex/archived_sessions`), and
nothing else under the home.

A rollout is append-only JSONL: Codex adds a line per event and never
rewrites one. Every line is `{timestamp, type, payload}`; from 0.155
there is also an `ordinal`, and a tool output carries a `metadata`
key. **A line has no id of its own** — unlike a Claude Code record,
which carries a `uuid` — so a row is keyed `<thread id>#<line number>`,
and the file being append-only is what makes that key stable. The
`type` vocabulary:

| type | what it is |
|---|---|
| `session_meta` | once, first: the thread `id`, `cwd`, `cli_version`, `originator`, `source` (`cli`, `exec`, `vscode`, or an object for a sub-agent), `git`, `base_instructions`, and on a sub-agent thread its `parent_thread_id`, `agent_nickname` and `agent_role` |
| `turn_context` | once per turn: `turn_id`, `model`, `cwd`, `effort`, the sandbox and approval policy, the `user_instructions` (the project's AGENTS.md) |
| `response_item` | the model-visible history. `payload.type` is `message` (`role` user / assistant / developer, `content[]` of `input_text` / `output_text` / `input_image`), `reasoning` (a `summary[]`, and `encrypted_content` the page cannot show), `function_call` (`name`, `arguments` as a JSON string, `call_id`), `local_shell_call`, `custom_tool_call` (`apply_patch`, with `input` the patch), `function_call_output` / `custom_tool_call_output` (`call_id`, `output` a string or content items), `web_search_call`, and a few housekeeping kinds (`ghost_snapshot`, `context_compaction`) |
| `event_msg` | what the UI was told: `user_message`, `agent_message`, `agent_reasoning`, `task_started` / `task_complete` / `turn_aborted`, `token_count`, `exec_command_begin` / `_end`. Every one repeats a `response_item` or describes progress — except `user_message`, which is the one record of what the person **typed** as against what Codex injected under the user role |
| `compacted` | Codex folded the history into a summary `message`; what the model saw from then on |
| `world_state` (0.155) | the whole context restated: the project's AGENTS.md, the environments, the collaboration mode |
| `token_usage_record` (0.155) | per-turn and per-thread token counts |

## Two generations, and the one thing they disagree about

**Which words the person actually typed** is the fact this provider has
to get right, and the two Codex generations record it differently:

- **Through 0.115**: a `user_message` **event** carries the text. A
  message under the `user` role says nothing about itself, so what
  Codex injected there (the project's AGENTS.md, an environment note)
  and what the person typed look alike.
- **From 0.155**: there are **no `user_message` events at all** — they
  became `item_completed`, which carries the same text inside
  `item.type: "UserMessage"` — and instead every message part is
  tagged in
  `internal_chat_message_metadata_passthrough.content_item_kinds`:
  `user.text` for a prompt, `agents_md.instructions`,
  `environments.environment_context`,
  `permissions.instructions`, `host_skills.instructions` and friends
  for what Codex put there.

So the provider reads the tags where they exist and the events where
they do not, and the title — the first typed prompt — comes from
whichever answered. The fixture holds a thread of each generation for
that reason: a build that reads only one signal titles half of them
`(untitled)`, or files an injected AGENTS.md as something the person
said, and only one of the two generations would catch it.

Measured on this machine: 20 rollouts from Codex 0.104–0.115, none
with a tool call, and one real 0.155.1 audit session with 18 `exec`
calls and 3 `wait` calls. A 0.155 `exec` is a `custom_tool_call` whose
`input` is **JavaScript** (`await tools.exec_command({cmd: …})`), and
its output is a list of content items rather than the JSON string with
an `exit_code` that 0.115 wrapped a shell result in; the renderer
reads both. `local_shell_call`, `web_search_call` and `compacted` have
still not been seen in the wild here — their shapes come from
`codex-rs/protocol/src/models.rs`.

A sub-agent thread — one Codex spawned from another — is its own
rollout file with its own thread id, naming its parent in
`session_meta`. It is its own transcript row and its own document, with
the parent's title in its display name, not a section of the parent.

## What the store holds

Two tables, no bookkeeping sidecar (a live thread's file is re-read
whole on every sync, and unchanged rows are free under doltlite's
content addressing):

- `transcripts` — one row per rollout. `id` is the thread id. The
  payload is what the file says about the thread as a whole: the
  fields off `session_meta`, the models the turns ran on, the title,
  the first and last timestamps, the counts by line type and by item
  type. Promoted columns: `parent_thread_id`, `cwd`, `git_branch`,
  `title`, `started_at`, `updated_at`, `rel_path`.
- `records` — one row per line, every type. `id` is
  `<thread id>#<line number>`; the payload is the line as written.
  Promoted: `transcript_id` (the render's diff bucket) and `line_no`
  (its order). Everything else is `payload->>'$.type'` and
  `payload->>'$.payload.type'`.

The **title** is the first `user_message` event's text, cut to a line
of at most 100 characters, or a sub-agent's nickname. Codex's own
`threads.title` in `state_5.sqlite` is the same first prompt on every
thread measured here; reading that database (and its `archived` flag)
is open, and the reason `path` is the home rather than `sessions/`.

**Transcripts are full of secrets.** A tool output can hold the
output of `env` or the contents of `.env`. The store keeps the line as
Codex wrote it, and the rendered document keeps the first
`max_tool_result_bytes` of each output. Nothing here redacts.

## Render

One document per thread, through chat-common. In line order:

- a `message` from the assistant is an **LLM Response**, by the model
  the turn's `turn_context` named; from the user, a **User Input** if
  it is what the person typed — its own `user.text` tag says so, else
  its text matches a `user_message` event, else it does not look
  injected (a leading tag, or the `# AGENTS.md instructions` heading)
  — and otherwise a **Harness Message** aside, as is anything under the `developer` role, cut at
  `max_tool_result_bytes` like a tool output (a permissions primer is
  4 KB, a model-switch note 9 KB);
- `reasoning` with a summary is an **LLM Thinking** aside; an
  encrypted one with no summary adds nothing, which on 0.155 is every
  one of them — the thinking is ciphertext and the summary is empty;
- `function_call`, `local_shell_call`, `custom_tool_call` and
  `web_search_call` are **Tool Call** asides, and the outputs **Tool
  Result** asides named after the call they answer (`(error)` when an
  older Codex's wrapped shell output carries a non-zero exit code),
  cut at `max_tool_result_bytes`;
- `compacted` is a System note;
- `event_msg`, `world_state` and `token_usage_record` lines render
  nothing.

Consecutive asides fold into one collapsed block. Stamps come from
each line's `timestamp` and never run backwards, though many lines
share one millisecond.

Ids are minted through `datalib_id` under `IdNamespace::Codex`, with
no account — a rollout names none — and each item's own stamp in the
id's leading bits, to the second the row stores it. A thread's
document carries no stamp: its row's is derived from its items, and an
older line arriving would re-key its `/chat/` URL.
`docs/dev/entity_ids.md` is the reference.
