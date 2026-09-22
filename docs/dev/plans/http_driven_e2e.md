# An interrupted first sync, driven through `datalib-http`

**Status: proposal (2026-09-22), not built.** §1 describes what the tree
does today and was checked against it at `473aef1f`. Where this doc and
the tree disagree, the tree wins.

## 0. The idea

The manual e2e golden
([`manual_e2e_live_sync_golden.rs`](../../../datalib/backend/dag/tests/manual_e2e_live_sync_golden.rs))
runs `datalib-dag` three times against the private live config: a full
first sync, an incremental second, and a reset-and-resync of one source
whose content tables must come back byte-identical. Every run is a clean
start-to-finish pass, and the test spawns the runner itself.

This plan changes two things about it.

**The first sync is interrupted, twice, before it is allowed to finish.**
Once by a crash, while every source is mid-fetch — wherever each
happens to be between two commits — and once by a person pressing Stop,
just after each source has sealed its first commit of that run. Then it runs to completion,
and the reset-and-resync at the end covers every source, not one. The
byte-identity check at the end is the oracle: a source that resumed by
skipping a page, or by fetching one twice, lands different rows.

**The test drives `datalib-http`, not the runner.** It presses the same
buttons the Manage tab presses — `POST /api/sync/jobs`, `POST
/api/sync/jobs/{id}/cancel` — and watches the same state the tab watches
— the SSE stream, the job rows, the run store. The Vue frontend stays
out: the seam under test is the API the frontend consumes, and that seam
is where the worker's cancel path, the job state machine and the
boot-time recovery live, none of which any end-to-end test exercises
today.

## 1. What the tree already has

Every verb the scenario needs is an endpoint, and every observation is
one too.

| the scenario needs | the tree has |
|---|---|
| press Sync all | `POST /api/sync/jobs {"kind":"all"}` — `datalib/backend/http/src/lib.rs`, `sync_enqueue` |
| press Stop | `POST /api/sync/jobs/{id}/cancel`. The worker sees the row flip to `canceled`, SIGTERMs `datalib-dag`, and SIGKILLs it after `CANCEL_GRACE` (15s). The runner forwards SIGINT to its steps; a step raises its `StopFlag`, seals at its next consistent point, and exits `cancelled` (`datalib_step/src/main.rs`). |
| press Reset | `POST /api/sync/jobs {"kind":"reset","source_ids":"a/ingest+blobs,b/ingest,…"}` → one `--reset` |
| watch a job | `GET /api/sync/jobs/{id}` (`SyncJobView`: state, `active`, `stopping`, pid), `GET /api/sync/stream` (SSE: every job transition plus `root` frames) |
| watch the run | `GET /api/runs/{run}/steps` — the run id is the job id, so a job's steps and their `RunState` are one GET away; `GET /api/runs/{run}/log` |
| watch a source | `GET /api/manage/rows` (per-source `StatusView`); `GET /api/pipeline/history?tree=<step>` — the store's commit log, each `Commit` with a UTC `date` and the `run` (job id) that made it (`history/src/lib.rs`) |
| commit often | `checkpoint_cadence = { at_most_every_secs = 1 }` at the top of the config (`dag/src/config.rs`); the runner hands it to every step |
| run the server as a process | `datalib/backend/http/tests/shutdown.rs` already spawns `$DATALIB_HTTP_BIN` on a temp root, reads `system/api-token`, and drives it over TCP |
| every binary side by side | `//datalib/backend:bin` stages `datalib-http`, `datalib-dag`, `datalib-step` and `datalib-applet`; the golden already takes it as `data` |

What the tree does **not** have, and this test would be the first to
exercise end to end:

- A run interrupted by anything other than a clean end. The cancel path
  has unit tests (`worker.rs`, `dag_run_state.rs`) and the runner's
  staleness rule says an aborted step is re-run (`scheduler.rs`, clause
  2, "never completed"), but no test has ever killed a live provider
  mid-page and asked whether the next run picked up where it left off.
- A step's store hit by SIGKILL between two writes, or between a write
  and its commit. Whether a doltlite store survives that is a question
  this test answers; a store that does not open afterwards is a finding,
  not a flake.
- The worker's recovery on boot (`worker::recover`), if the server is
  ever the thing killed (§4).

## 2. The scenario

Phases, in order. Each phase's wait is a poll with a deadline against
the observable named, never a sleep (AGENTS.md § "Tests wait on the
observable"). A deadline that passes fails the phase and names what
never arrived.

```
config: checkpoint_cadence 1s

P1  POST sync all                                    → job A
    wait: every ingest step is `running` or already terminal
          in /api/runs/A/steps
    kill: the whole process tree under datalib-dag's pid (§3)          ← crash
    wait: job A terminal (the worker sees the child exit; state `failed`)

P2  POST sync all                                    → job B
    wait: every ingest tree has a commit with run == B
          (GET /api/pipeline/history?tree=<step>)
    POST cancel B                                                       ← Stop
    wait: job B `canceled`; every step in /api/runs/B/steps is
          `stopped` or a success state, never `running`

P3  POST sync all                                    → job C
    wait: job C `done`; every step succeeded
    snapshot the run summary as today

P4  POST sync all                                    → job D   (the incremental run 2 of today)
    wait: `done`; assert the skipped/incremental statuses as today

P5  read every ingest store's content tables          → before
         (entities.doltlite_db, and blobs.doltlite_db where it exists)
    POST reset, source_ids = every ingest step, each with +blobs
    wait: `done`
    POST sync all                                    → job F
    wait: `done`; every step succeeded
    read the same tables again                        → after
    assert before == after, path-level diff as today
```

P1 does not try to catch a source between a write and its seal. With
a 1s cadence that window is a second wide, the API cannot see it
(history shows commits, not the working set), and it is not the
realistic crash anyway: a machine goes away wherever each step happens
to be. Killing once every ingest step has reported `running` lands the
crash between two commits of each, and the rows since each step's last
seal are what the resume has to cope with.

P2 keys its wait on the commit's `run` field rather than on a
timestamp: every step stamps ` run=<id>` into its commit message and
`datalib_history` parses it back, so "job B has sealed something in
this tree" is an exact question with no clock in it.

P5 says `+blobs` on every ingest step. `reset_store` on a path that
does not exist is a no-op (`doltlite_raw.rs`), so a source without a
CAS resets its entities and skips the rest, and the test derives
nothing. The diff covers the blob store too where there is one: an
entities table that came back identical because its blobs were never
re-fetched would pass for the wrong reason.

P1's wait is racy by construction — a fast source can finish before a
slow one has a row — and the test accepts that: it waits for "has rows
**or** has finished" per source, logs which sources were caught
mid-flight, and moves on. This test already depends on a dozen live
upstreams and a keyring; the clock is one more known dependency, and a
phase that catches three sources mid-flight on one bake and five on the
next is still exercising the code the clean run never does. The one
thing it must not do is fail because a source was too fast.

P2's "a commit from this run" is what makes the cadence knob
necessary: at the default 15s the fast sources would finish before
sealing anything, and Stop would arrive at a run with nothing left to
interrupt.

## 3. Two different kills

Stop and crash are not the same fault, and the test needs both.

**Stop (P2) is the product's path**: cancel → SIGTERM the runner →
SIGINT the steps → each seals and exits `cancelled`. It tests that a
step really does stop at a consistent point, that the runner records
what each step did, and that the next run treats `cancelled` as
"never completed" and runs it again.

**Crash (P1) is SIGKILL to the runner *and* its steps.** Not the
runner alone: `subprocess.rs` spawns each step with `kill_on_drop`,
which runs when the runner exits normally and never when it is
SIGKILLed, and the runner removes `DATALIB_PARENT_PIPE` from the step's
environment on purpose, so a step does not notice its runner dying.
SIGKILL the runner alone and its steps carry on writing their stores
while the next job's runner starts — two writers on one file, the
exact thing `etl/README.md` § "Connection pools" forbids. That is a real
gap (§6), but it is not the fault P1 is after. P1 simulates the machine
going away: kill every process in the tree, in one sweep, children
first so no step outlives the sweep.

The pid the test kills from is the runner's, on the job row
(`SyncJobRow.pid`, set by the worker so cancel has something to
signal); the steps are found by parent pid (`pgrep -P`, on macOS and
Linux alike). A helper that walks the tree and SIGKILLs it leaf-first
is twenty lines and belongs in the test. Its one guard: the runner's
own parent (`ps -o ppid= -p`) must be the `datalib-http` the test
spawned, or it refuses — it only ever kills under a process it started.

## 4. Where the server sits

The server is a child of the test, spawned as `shutdown.rs` does it,
and it lives across every phase. Killing it too would test
`worker::recover` — the boot that finds a `running` job whose runner is
gone and reads the run store to say what became of it — and that is
worth a phase of its own later (§7), not on the first pass: a server
restart mid-scenario adds a second port, a second token and a second
set of failure modes to a test that is already long.

The runner is a child of the server, and the server's parent pipe to
it is what makes the runner exit if the server dies. The test never
sends the runner a signal directly except in P1.

## 5. Shape of the code

The phase script is data; the driver is the imperative shell
(`docs/dev/style.md`). One new integration test, no new product
crates.

```
datalib/backend/http/tests/
  live_sync_interrupted.rs         the test: config rewrite, phases, oracles
  support/server.rs                Server (lifted from shutdown.rs): spawn, token,
                                   GET/POST helpers, an SSE reader on a thread
  support/kill.rs                  process-tree SIGKILL
```

- **`Server`** moves out of `shutdown.rs` into `tests/support/` so both
  tests use one copy. Nothing in it changes.
- **Phases as values.** A `Phase { enqueue: JobKind, wait_for: Wait,
  then: Action }` list, with `Wait` an enum (`EveryIngestRunning`,
  `EveryIngestCommittedIn(job)`, `JobTerminal`) and `Action`
  (`KillTree`, `Cancel`, `Nothing`). The driver runs the list; a unit
  test over the list is not the point, but the shape keeps the driver
  short and the scenario readable at a glance.
- **Observing a store.** Row counts and content tables come from
  opening the `.doltlite_db` read-only with sqlx, as the golden's
  `content_tables` does today — the test is a doltlite-linked binary.
  Which run sealed a commit comes from `GET /api/pipeline/history`,
  which is what the UI shows.
- **Oracles**, all lifted from the golden: `assert_step_statuses_ok`
  over `run_summary` (P3, P5), the incremental-run assertions (P4), and
  `json_diff_paths` over `content_tables` (P5), now over every
  `*/ingest/entities.doltlite_db` and `blobs.doltlite_db` rather than
  one.
- **Skips by env**, for the second bake of the day: `DATALIB_E2E_PHASES=P3,P4,P5`
  runs a plain bake against a root the previous run left. The
  interrupted phases are the expensive part to re-run when only the
  oracle moved.
- **Tags and driver**: `manual`, `external`, `no-sandbox`, like the
  golden; `manual_e2e_run.sh` gets a `--interrupted` mode, or this
  replaces the golden's run 1 and run 3 outright once it is trusted
  (§7).

### The hermetic twin

The same driver runs against the TNG fixture. `tests/fixtures/`
ingests every source hermetically from playback fixtures and local
files (`run_sync_pipeline.py`), so a config that names those sources
with `checkpoint_cadence` 1s and a P1 that kills mid-walk is a CI test
— minutes, no keyring, no quota — with the same phases and the same
oracles. It cannot catch what the live one catches (an upstream that
paginates differently from its fixture), but it catches every
regression in the worker, the runner and the step's stop path on every
PR, and it is where the driver gets debugged before it meets a live
config. Build the driver against the fixture first; point it at the
private config second.

## 6. What the test is likely to find

Named here so a red first run is read as a finding, not as a broken
test.

1. **A provider whose cursor lands before its rows.** A page written
   after its cursor is committed is a page the resumed run skips; P5's
   diff shows it as missing rows. The fix is per provider
   (`data_architecture_ingestion.md` § "When the cursor swallows a
   config change" is the neighbouring rule).
2. **A truncate-and-refill provider caught mid-refill** (`whatsapp`,
   `pdf`, `fsindex`): `checkpointer.rs` says such a provider must not
   ask to seal until its refill is done. P1's SIGKILL does not ask; the
   next run must cope with a store holding half a refill.
3. **A doltlite store that does not open after SIGKILL.** If the working
   set can be left inconsistent by a kill between writes, that is a
   storage-engine finding and the biggest one this test could produce.
4. **Orphaned steps after a runner-only crash** (§3). Not what P1
   tests, but the reading of `subprocess.rs` above is a product gap on
   its own: a runner SIGKILLed by the desktop shell, or by the OOM
   killer, leaves its steps writing. The fix is a process group, or a
   runner-to-step parent pipe the way the server has one to the runner.
   Filed as #657, which also shows the product's own Stop reaches it:
   the worker SIGKILLs the runner at 15s and the runner never escalates
   on one signal.
5. **The graces that don't nest.** The step gives itself 10s
   (`INTERRUPT_GRACE`) to reach a consistent point, the worker gives the
   runner 15s (`CANCEL_GRACE`) before SIGKILL; a step that seals at 9s
   leaves the runner 6s to record it, on a laptop that is also running
   every other step. P2 will show whether that margin holds. #657 asks
   for graces that nest with room.
6. **Steps launched after Stop.** The runner's signal handler SIGINTs
   the steps running at that instant and tells the scheduler nothing,
   so a step that becomes ready afterwards is launched, never signalled,
   and hard-killed with the runner at 15s (#657). On a config with more
   sources than parallel slots P2's "never `running`" assertion fails
   on those steps every time. That is why §7 puts the scheduler fix
   first.

## 7. Plan of record

0. **The scheduler learns about Stop** (#657, the separable half): the
   first signal sets a flag the scheduler reads before every launch, so
   the run drains instead of proceeding, and the runner exits on its
   own once the in-flight steps have stopped — inside the worker's 15s,
   so its SIGKILL never fires. With a test that a step ready *after*
   the signal is recorded, not launched. P2 cannot pass reliably
   without this, and once it lands P2 is its regression test.
1. **Driver on the fixture** — `Server` into `tests/support/`, the
   process-tree kill, the phase list, the three oracles; a hermetic
   `rust_test` on a fixture-shaped root with a 1s cadence. This is the
   PR to get right; everything after is pointing it somewhere else.
2. **Live config** — the `manual` target, the `--interrupted` mode in
   `manual_e2e_run.sh`, `DATALIB_E2E_PHASES`. First bake; file what §6
   turns up.
3. **Retire the golden's run 1 and run 3** once the interrupted test's
   P3–P5 have produced the same snapshots twice. One live test, not two
   that overlap.
4. **Later: kill the server too** — a phase that SIGKILLs `datalib-http`
   between P1 and P2 and asserts `worker::recover` closes job A with
   the right state and message. Separate PR; it is the only phase that
   needs a second server instance.

## 8. Open questions

- Whether the process-group half of #657 lands before or after step 1.
  If before, P1's kill helper shrinks to one `kill(-pgid)` and the test
  can also assert that killing the runner *alone* takes the tree with
  it. If after, the helper walks the tree itself and that assertion is
  added when the fix lands.
- How P4's incremental assertions read a run that follows an
  interrupted first sync. Today's golden asserts run 2 against a clean
  run 1; after P1–P3 a source may have been fetched in three pieces, and
  `refresh_window_days`-style re-queries may see different counts. Find
  out on the first bake rather than guess.
