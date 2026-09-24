# The TNG fixture through the server, then a fuzzer over it

**Status: step 1 is built; steps 2–4 are a proposal (2026-09-24).**
Rewritten after the supervisor landed: the first version of this plan
(#659) drove `/api/sync/jobs`, which no longer exists. Where this doc
and the tree disagree, the tree wins.

## 0. The idea

Two tests, one harness.

1. **The fixture is built the way a person builds a root.** The
   `:ingested_tng` genrule starts `datalib-http` on an empty root and
   drives it over the API the web client uses: the starter config from
   `POST /api/config/init`, one `PUT /api/config` per source added, and
   every sync a `POST /api/requests`, followed on `GET /api/requests`
   until it closes. Everything downstream of the fixture — the goldens,
   the render contract, the Playwright suite — then rests on the
   server's path, not only the terminal's.
2. **A fuzzer pounds on that server.** Random clicks — sync one source
   or all of them, Stop, Pause, Resume, Reset — interleaved with random
   process deaths, and at the end one plain sync to completion. Then a
   reset of every source and a sync from scratch, and the two must
   agree. A step that resumed by skipping a page, fetching one twice, or
   trusting a half-written store lands different rows, and the diff
   names them.

## 1. What step 1 built

- `datalib-http --now <RFC 3339>` pins every sync's "now", as
  `datalib-dag --now` does for one run. Ingest reads the wall clock
  either way; it is render and the index that stamp `now`, and without
  the pin the fixture would change every day and re-run the qmd embed
  downstream of it.
- `tests/fixtures/sync_drivers.py` has two drivers with the same four
  calls (build the config, sync, reset, close). `run_sync_pipeline.py`
  takes the server as an optional 30th argument; the genrule passes it,
  `ingested_tng_test` and `render_contract_test` do not, so both doors
  run over the same fixture on every build.
- The second Slack capture used to arrive by changing an environment
  variable between runs. A server fixes its steps' environment when it
  starts, so the playback path is now a link the script repoints — a
  changed upstream behind the same address.
- qmd is off in the fixture's config, as a person turns it off from
  Manage; `:ingested_tng_qmd` embeds separately, as before.

Measured on a warm mac: the whole script takes 7s through
`datalib-dag` and 4s through the server. The two runs produced the same
markdown file for file and the same grid index row for row once commit
hashes and wall-clock stamps are masked (those differ between any two
runs of either door); the only other difference was ten Storage rows
one byte apart, which is store size on disk.

## 2. The fuzzer

A `py_test` in `tests/fixtures/`, on the same prepared root and the
same `HttpDriver`. One seeded random schedule per case; the seed is
printed first and taken from `TNG_FUZZ_SEED` when set, so a red run
replays.

**What a person does**, each an endpoint:

| event | call | acceptable answers |
|---|---|---|
| sync everything | `POST /api/requests {}` | 200 |
| sync a few sources | `POST /api/requests {roots}` | 200 |
| Stop | `POST /api/requests/<open id>/stop` | 204 |
| Pause a step | `POST /api/steps/<id>/pause` | 204 |
| Resume | `POST /api/steps/<paused id>/resume` | 204 |
| Reset some sources | `POST /api/reset {targets: [<id>+blobs…]}` | 204, or 409 while a sync runs |
| Save the config unchanged | `PUT /api/config` | 200, `ok` |

**What goes wrong underneath**:

| event | how |
|---|---|
| a step dies | SIGKILL one child of the server (`pgrep -P`), whatever it is doing |
| a step's whole group dies | SIGKILL its process group |
| the server dies | SIGKILL `datalib-http` and start it again on the same root; the new one's `take_over` closes what the dead loop left open |
| an upstream misbehaves | later: the hostile playback cases of #644 (empty 200, truncated page, 429 mid-walk) as events |

Between events, a gap drawn from the seed — from nothing to about a
second, so events land both between steps and in the middle of one. The
gap is the fuzz input, not a wait for something: the AGENTS.md rule
about sleeping applies to ordering, and nothing here is ordered.

**Checked after every event**: no 5xx; the server is up unless the
event killed it; every request `GET /api/requests` shows is open or
closed with an outcome.

**At the end**: resume every pause, sync everything, and require the
request `done` and every step fresh. Snapshot: the content tables of
every `*/ingest/*.doltlite_db`, every render store's documents, and
`grid_rows`, with stamps and commit hashes masked as in §1. Then reset
every ingest step `+blobs`, sync everything again, snapshot, and diff.
Also: no process of ours outlives the server's shutdown.

**Budget**: at 4s per clean sync, a case with 40 events and the two
closing syncs is well under a minute. CI runs a handful of fixed seeds
(new schedules come from changing the list, on purpose); a `manual`
target runs random seeds until stopped, for soaking.

## 3. What it is likely to find

A red first run is a finding. Named in advance:

1. **A provider whose cursor lands before its rows**: the resumed run
   skips a page, and the diff shows missing rows (#644 attacks the same
   bug from the provider side).
2. **A truncate-and-refill provider caught mid-refill** (`whatsapp`,
   `pdf`, `fsindex`): a SIGKILL does not wait for the refill to finish.
3. **A doltlite store that does not open after SIGKILL** between two
   writes. The biggest one this could produce.
4. **A step that outlives its loop** when the server is killed: each
   step runs in its own process group, so a dead server leaves them
   running unless something takes them down.

## 4. Plan of record

1. **The fixture through the server.** Built (§1).
2. **The fuzzer on the fixture**, the person's events and step deaths
   first; server death once the first seeds are green.
3. **Upstream faults as events**, sharing #644's hostile playback.
4. **The live config**: the same driver pointed at the private config
   (`manual_e2e_live_sync_golden.rs` today), with a short fuzz before
   its first sync. Retire that golden's run 1 and run 3 once it has
   produced the same snapshots twice.
