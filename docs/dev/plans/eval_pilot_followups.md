# What the Minds evals pilot found, and what is left to fix

**Status: partly built 2026-11-03.** Merged: #1530 (a thread root's
reply count), #1531 (the standalone ingest binaries deleted;
`datalib-step` by hand documented instead), #1532 (agent docs in
`datalib-step help`, checked; the four doc errors; the idle `status`
line), #1533 (Slack `archived`, pages of 999, three requests at once),
#1534 (a failed pass is an error line and a warning), #1535 (what a
stopped or failed step owes), #1538 (a network that goes away ends a
download `incomplete`; every web source, not only Slack). The PR that
lands this file: Slack looks up owed authors before its walk too;
`pull-runtime` reads latchkey's version without running it and takes
`--into <dir>`. Thad took decisions 2, 4 and 12 and deferred 1
(`replies`); on 4 he added that diff groups and the time-series index
are young and need not be preserved as they are. Still to do: decision
4's PR, §2 PR 5's transaction cost and `client.counts` fallback, and
§3's decisions 3, 5-11 and 13. An audit of main at
`f5533f7a4` against the evals team's reports (v0.40.0 to v0.48.0) and
datalib#1471. §1 says what is already fixed and how we know; §2 is the
work, as PRs; §3 lists the decisions Thad has to make before the PRs
that need them. Line numbers are as of `f5533f7a4`.

## 1. Already fixed (checked in the code and its tests)

| Report | Fixed by | The test that would fail on the old code |
|---|---|---|
| A `since` before 1970 sent a negative `oldest` and mirrored nothing (#1048) | #1057 | `slack/src/ingest/mod.rs` unit test on the clamp |
| A sync that mirrored nothing read as a clean success (#1049) | #1162 | `slack/tests/.../run_problems.rs` |
| Channels created after the first sync were never mirrored (#1477) | #1483: no TTL on `conversations.list` | `new_since_last_sync.rs:68` |
| New posters had no `users` row (#1471 §1) | #1483: `users.info` for an unknown author | `new_since_last_sync.rs:98` |
| The step message said `msgs=0` on a sync that stored 416 (#1471 §6) | `2d963aba9` (v0.41.0) | `step_tests/final_message.rs` (on SMS, not Slack) |
| A re-fetched thread pruned its own replies (#1471 §9: real data loss on v0.40) | `f1dcbf903` (v0.41.0) | `history_prune.rs:224` |
| A refresh with nothing new re-rendered and re-indexed | #1498 (on main, **not in v0.48.0**) | Slack quiet-channels test, on content tables |
| A routine sync re-walked every channel | #1429: quiet channels skipped | `quiet_channels.rs` |
| No starter config for an agent | `datalib-dag init`, `datalib-step examples <type> [--append]`, `datalib-step help` | `append.rs`, `source_docs.rs`, `config_examples_test.rs` |
| `--check` not in the guide | `agent_user.md` §setup step 4 | — |

#1498, #1496 and #1510 merged after v0.48.0, so the evals team
sees none of them until the next release.

## 2. The work

### PR 1: docs that are wrong or missing (no decisions)

- `step_protocol.md:657-669`: the Python example skips every input not
  in `DATALIB_DAG_CHANGED_INPUTS`, so it does nothing on a first run,
  which is exactly the bug the agents hit. Treat an empty list as "all".
- `step_protocol.md`: a section for the consumer. You may be started
  while a streaming input (every built-in ingest) is still writing, and
  you will be run again when it seals. Read the commit you were started
  against, never `main`. A failed input's last commit is what you read.
- `building_on_datalib.md:286-288`: `DATALIB_READS` holds
  `<fingerprint>:<commit>`, not a commit hash
  (`subprocess.rs:1139` asserts the form). "Following changes"
  (`:307-313`) reads `dolt_hashof('main')`; switch it to the pinned
  commit.
- `building_on_datalib.md:445` tells you to wait until `status` shows
  no open request, which is where the `grep running` loop came from.
  Say that `datalib-dag <config> --sync <step>` blocks and exits
  0/2/130, and make that the wait idiom.
- `building_on_datalib.md`: a paragraph for a step that reads a service
  with no history. Re-query with an overlap, dedupe by id and update
  stamp, and never move the watermark past what came back. Point at the
  built-in `linear` source.
- `agent_user.md` and `help slack`: `since` is a choice, and no `since`
  means 2024-01-01, not everything. The complete examples
  (`complete_api.toml:33`, `all_sources.toml`) say "set this to your
  data's oldest day" beside their dates.
- `agent_user.md`: one troubleshooting line, saying the
  `client.counts`/`saved.list` warnings cost two tables and nothing
  else (until PR 3 quiets them).
- `slack/INGEST.md:5` says `slack-ingest` commits, and
  `FetchOptions::sealer` (`mod.rs:1344`) says `None` commits once at
  the end. Neither is true (PR 7). `INGEST.md:28-31` says a refused
  `client.counts` costs nothing else, but it also turns off the
  quiet-channel skip.
- `agent_user.md:380` assumes `~/.local/bin`.
- `datalib-step help agent` rewrites relative links to `blob/<sha>`
  (`help.rs:119`), the large HTML pages the agents choked on. Point
  them at the installed `docs/` when it exists, else
  `raw.githubusercontent.com`.

### PR 2: `datalib-dag` says what happened (no decisions)

- **A failed pass is kept.** When a step's pass fails and the step is
  due again, `round.rs:504-505` drops the failure. `PassEnd` carries
  the exit code but not the error. Put the status and error on
  `PassEnd`, write the error-level line for it in the run store, and
  keep a count of failed passes on the step's row. The test: a step
  that exits 1 on its first pass and 0 on its second leaves the
  failure in the run store.
- **`changed` on stdout is the run's.** `datalib_dag.rs:397-406` prints
  the per-pass `StepStatus`, measured since the last seal. Print the
  run-level `changed` that the `run_summary` event already computes
  against the run's start (#1114).
- **`status` says idle without saying "running".** Change the idle line
  (`datalib_dag.rs:761`) to `idle: no open requests`, and add
  `status --json`. A `wait` verb is in §3.
- **A render warning reaches the summary.** These paths only `warn!`
  and drop a row or a document: `whatsapp_render/.../parse.rs:127,187,205,311`,
  `beeper_render/.../normalize.rs:49`, `signal_render/.../normalize.rs:35`,
  `apple_messages_render/src/render.rs:528`,
  `email_render/.../render.rs:213`, `garmin_render/.../render.rs:282`,
  `etl/render/src/indexed_markdown.rs:836`, `etl/render/src/inputs.rs:139`.
  Route each through `RenderCtx::report_*`. (We could not identify the
  exact error the evals team saw; Slack's render has no warn or error
  call.)

### PR 3: Slack stops quietly losing a run (no decisions)

- **Offline is retried, then reported.** `web/src/http.rs:309` retries
  curl exits 7, 28, 35 and 56, but not 6 (could not resolve host),
  which is what a laptop waking from sleep returns. Add 6, 52 and 55.
  A terminal error also resets the no-progress clock (`http.rs:595`);
  it should not.
- **A run that gives up ends `incomplete`.** Slack never calls
  `gave_up`. The thread, file and user loops run with
  `failures_in_a_row: 0`, and history does not go through `owed`, so
  #1193's "incomplete" never fires for Slack, though #1193 says it
  covers Slack. Give each loop a budget, and stop the frontier after K
  consecutive `Unavailable` pages. The test: a playback that answers
  curl exit 6 from the third request on ends the step `incomplete`
  with problem rows.
- **The optional listings stop warning every run.** A gateway 403 on
  `client.counts`/`saved.list` becomes an error row
  (`download_problems.rs:325-328`); `unknown_method` a warning. Both
  repeat every sync. Remember a refusal in `sync_scope_state` and stop
  calling for a while; whether a remembered refusal still shows a row
  is in §3.
- **Users before the walk.** Run `fetch_users_owed` before the frontier
  as well as after, so a stopped run does not leave `users` rows with
  no payload.

### PR 4: Slack scope and visibility (after §3 decisions 1-3)

- `replies = true | false | ["channel", …]` in `slack_config`, as a
  filter on the owed threads (`mod.rs:1775`). With replies off, a first
  sync of a large workspace gets every channel's top-level history in
  minutes rather than hours. Parked as #1059; port it, don't revive the
  branch.
- `archived = true`, which feeds the `include_archived` flag that
  already exists (`mod.rs:144,188`, `db.rs:316-330`); today only named
  channels can be archived ones. A wizard entry beside it.
- The render shows a thread's reply count. Nothing under
  `slack_render/` reads `reply_count` or `latest_reply`, so a root whose
  replies are still owed looks like a plain message. Show "N replies,
  M not mirrored yet" on the root, and put the count on its grid row.
- What is owed, at rest: each source reports an `owed{kind}` gauge at
  the end of a run, which the runner does not zero
  (`runs_sink.rs:339-341` zeroes `queued`). Shown on the Manage row and
  in `status`.

### PR 5: Slack goes faster (measure first)

- History pages of 999 (`mod.rs:1042`, now 200), and two or three
  pages in flight across different channels. The pilot measured no
  429s on history at 3 in flight; measure on a real workspace before
  merging.
- One SQL transaction per seal rather than per page and per thread
  (`db.rs:402`, `flush: 1` at `mod.rs:1131`), on `raw_store::Batch`
  as Linear does. This is most of the file growth the pilot measured.
  Collecting garbage at the end of every ingest is #951; this plan
  does not duplicate it.
- A channel `client.counts` cannot name is walked every run
  (`Newest::Unknown`, `mod.rs:1752`), which is every channel against a
  server or token without it. Fall back on `conversations.list`'s
  `updated` for channels. Needs a live check that it moves on a post.

### PR 6: the runner's contract with a consumer (after §3 decisions 4-8)

Whichever of decisions 4 to 8 are taken, each with a supervisor-harness
scenario and a line in `step_protocol.md`. Decision 4 alone ends most
of what the agents saw: a no-op Slack sync would wake nobody.

### PR 7: the runtime and the standalone ingest binaries

- `pull-runtime` (`datalib_step/src/main.rs:728-746`) runs
  `latchkey --version`, which looks up an encryption key first, fails
  with no key and no keychain, and on a mac can create a key. Read the
  version from `node_modules/latchkey/package.json` instead. The test
  (`runtime_fetch.rs:107-115`) uses a fake `node` and cannot see this.
- `pull-runtime --into <dir>` so an install can put `runtime/` beside
  its binaries, where the resolver already looks, independent of
  `HOME`.
- The provider ingest binaries (`slack_ingest`, and the notion,
  github, gitlab, chatgpt, claude and jmap ones) never commit, so the
  next open discards what they wrote. None ships. Delete them (§3).

### Outside this repo

- **default-workspace-template**: its `SKILL.md:159` resolves links
  against `github.com/.../blob/v0.48.0/docs/`, the large HTML pages;
  use the docs it installs. Install under `/opt` with links in
  `/usr/local/bin` rather than `$HOME/.local`. `qi-imbue/datalib-inspiration`
  is pinned to v0.42.0 and no longer what workspaces get.
- The harness items (a skill written after the chat starts is not
  offered; `required_skills` passes on a read) are the evals team's.

## 3. Decisions for Thad

1. **Replies as declared scope**: `replies = true | false | [channels]`,
   default true. Or rank every thread after all history while a
   channel has no coverage yet, with no knob.
2. **Archived channels**: an `archived` option, default off.
3. **"Fetch this first"**: a one-run priority input to the frontier
   (named channels or threads), or leave it to stop and a scoped
   restart.
4. **What a raw store's version is.** *Taken: the version moves only
   when content (or `problems`) moved; diff groups and the time-series
   index may change to suit.* Today it is the head commit,
   which moves on every sync because each run writes a `sync_runs` row,
   so a custom step reading `slack/ingest` runs on every refresh. The
   proposal: an ingest reports the last commit at which a content table
   (or `problems`) moved. Diff groups and the time-series index name
   commits, so this needs a look at both.
5. **A consumer that wants its inputs settled**: a per-step
   `start = "after_inputs_settle"`. Today the only way is
   `reads = "files"`, which also locks writers out while it runs.
6. **First run**: set `DATALIB_DAG_CHANGED_INPUTS` to every input on a
   first run, so an empty list can only mean nothing moved; or add
   `DATALIB_DAG_FIRST_RUN=1`. Either changes the step protocol.
7. **A failed input**: tell the consumer (`DATALIB_DAG_FAILED_INPUTS`),
   hold it (`hold_on_failed_inputs`), or both.
8. **`datalib-dag wait [request]`**: a verb that blocks until a request
   closes and exits with its outcome, beside `GET /api/requests/{id}`
   (#1524).
9. **The 6-hour TTL on `users.list` and bookmarks**: drop it (one or two
   pages), or make it a param.
10. **A remembered refusal of `client.counts`/`saved.list`**: still a
    row (info), or nothing.
11. **No `since`**: everything, or 2024-01-01 as now.
12. **The standalone ingest binaries**: delete, or wrap each in
    `RawStoreSession`.
13. **Saying who is calling**: Slack requests carry Chrome's
    User-Agent on purpose. An `X-Datalib-Step` header sent only on the
    gateway path, which the gateway strips, needs latchkey's help.
