# The TNG fixture through the server, then a fuzzer over it

**Status: proposal (2026-09-24), checked against the tree on
2026-11-27. Step 1 is built (#770): the TNG fixture is synced through
`datalib-http`. Step 2 is built for the person's events and step
deaths (#774), and the fuzzer is still `manual`; server deaths, step 3
(upstream faults) and step 4 (the live config) are not built.** The
first version of this plan (#659) drove `/api/sync/jobs`, which no
longer exists.

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
   or all of them, Stop, turn a step off or on, Reset — interleaved with random
   process deaths, and at the end one plain sync to completion. Then a
   reset of every source and a sync from scratch, and the two must
   agree. A step that resumed by skipping a page, fetching one twice, or
   trusting a half-written store lands different rows, and the diff
   names them.

## 1. The fixture through the server: built

Built in #770. The `:ingested_tng` genrule drives `datalib-http` over
its API through `tests/fixtures/sync_drivers.py`, and
`:ingested_tng_test` runs the same driver through `datalib-pipeline`,
so both doors run over the same fixture on every build
([`tests/fixtures/README.md`](../../../tests/fixtures/README.md) has
how). `datalib-http --now` pins every sync's "now", as
`datalib-pipeline --now` does. When it landed, the two doors produced
the same documents and grid rows once commit hashes and wall-clock
stamps were masked.

## 2. The fuzzer

`tests/fixtures/tng_fuzz_test.py`, on the same prepared root and the
same `HttpDriver`. One seeded random schedule per case; the seed is
printed first and taken from `TNG_FUZZ_SEED` when set, so a red run
replays.

**What a person does**, each an endpoint:

| event | call | acceptable answers |
|---|---|---|
| sync everything | `POST /api/requests {}` | 200 |
| sync a few sources | `POST /api/requests {roots}` | 200 |
| Stop | `POST /api/requests/<open id>/stop` | 204 |
| Turn a step off | `POST /api/steps/<id>/turn_off` | 204 |
| Turn it on | `POST /api/steps/<turned-off id>/turn_on` | 204 |
| Reset some sources | `POST /api/reset {targets: [<id>+blobs…]}` | 204, or 409 while a sync runs |
| Save the config unchanged | `PUT /api/config` | 200, `ok` |

**What goes wrong underneath**:

| event | how |
|---|---|
| a step dies | SIGKILL one child of the server (`pgrep -P`), whatever it is doing |
| a step's whole group dies | SIGKILL its process group |
| the server dies | not yet: SIGKILL `datalib-http` and start it again on the same root; the new one's `take_over` closes what the dead loop left open |
| an upstream misbehaves | later: the hostile playback cases of #644 (empty 200, truncated page, 429 mid-walk) as events |

Between events, a gap drawn from the seed — from nothing to about a
second, so events land both between steps and in the middle of one. The
gap is the fuzz input, not a wait for something: the AGENTS.md rule
about sleeping applies to ordering, and nothing here is ordered.

**Checked after every event**: no 5xx; the server is up unless the
event killed it; every request `GET /api/requests` shows is open or
closed with an outcome.

**At the end**: turn every step back on, let every open request close, sync
everything and require it `done`. Snapshot every table of every store
and every document, with stamps, commit hashes and clock-minted (v7)
uuids masked. Then reset every ingest step `+blobs` and every render
step, sync everything again, snapshot, and diff, pairing rows by `id`
so a changed row reads as its changed columns.

Left out of the comparison, each because it is about the runs rather
than the data — a store that has been through a storm has had more of
them than one synced once:

- `sync_runs`, `_datalib_meta`, and the index's `source_cursors`;
- `attempt_count` in a `*_bookkeeping` table;
- `volatile_payload`, churn by construction;
- Datalib's measurements of its own stores (`source_measurements`, the
  `datalib` grid rows, each source's `_datalib/storage.md`): a reset
  keeps a store's history, so the file is bigger after one.

Seed 0 plays no events: the comparison alone, so a red there is the
oracle and not the storm. It is what these exclusions were calibrated
on.

**Budget**: a case with 40 events and the two closing syncs takes about
30s on a warm mac. The target runs seeds 0–3, one per shard; new
schedules come from changing the list, on purpose. `TNG_FUZZ_SEED=<n>`
replays or explores one. It is `manual` — run it with
`bazelisk test //tests/fixtures:tng_fuzz_test` — and is not in CI. Its
BUILD comment says it waits for the Manage screen to start and stop
single steps; a request already names its roots and a step can be
turned off, so that reason may be out of date, and joining CI is the
open part.

**Kills often find nothing to kill.** A kill with no step running asks
for a sync of a few sources and waits up to 3s for one to start; when
those sources are already fresh nothing starts, and the kill is
skipped (a quarter to a half land). Making it land every time — reset
the source first, or pick a stale one — is the next thing to fix.

**Found so far**:

- Every `/api/…` path that named no endpoint answered 200 with the
  app's page — the SPA fallback — so a mistyped call, or a step id with
  its slash unencoded, read as success. Now a 404 (`embed.rs`).
- A store killed during its first open — tables made, schema not yet
  committed — could never be written again: at doltlite's
  initialization commit neither `dolt_reset --hard` nor `dolt_clean`
  clears an `AUTOINCREMENT` table and its `sqlite_sequence`
  ([`doltlite.md`](../doltlite.md#plain-sqlite-files-and-sqlite-compatibility)).
  Seeds 2 and 3 hit it under `bazelisk test`,
  where four shards at once slow each ingest enough for a kill to land
  that early. Fixed in #776, with a test that builds the state directly.

A red storm becomes a fast, deterministic test first: read the state
out of the error (which table, which status, how many commits), build
it by hand in the owning crate's tests, watch it fail, then fix. The
fuzzer confirms; it is not how a finding gets debugged.

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

1. **The fixture through the server.** Built (#770, §1).
2. **The fuzzer on the fixture**: built (#774), with the person's
   events and step deaths. Left: server death, which needs a second
   server on the same root after the first is killed; kills that land
   every time (§2, "Kills often find nothing to kill"); and the target
   in CI.
3. **Upstream faults as events**, sharing #644's hostile playback.
   Playback answers inside `datalib_etl_web` (`playback.rs`), before latchkey is
   spawned, so a fault injected there never crosses the credential
   path. The sign-in suite (`datalib/ui/tests/e2e_auth/`, 2026-10-05)
   has the other door: a fake internet behind the real `latchkey curl`
   and router (`fake_sites.mjs`). A fault that matters to the person —
   an expired credential, a 401 mid-walk, a Cloudflare 403 — belongs
   there; one about our own resume logic (a truncated page, a 429)
   is fine on playback.
4. **The live config**: the same driver pointed at the private config
   (`manual_e2e_live_sync_golden.rs` today), with a short fuzz before
   its first sync. Retire that golden's run 1 and run 3 once it has
   produced the same snapshots twice.
