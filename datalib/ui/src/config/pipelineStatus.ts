// What a pipeline row's Status column says, and why.
//
// Pure functions over three inputs — the config's steps, the job queue,
// and the runner's per-step record — so the whole state machine is
// testable without a grid, a browser, or a running sync. Manager2View
// does the drawing; everything about *which* state a row is in lives
// here.
//
// The three sources and their precedence are the substance of this
// file; see `stepStatus`.

import type { ConfiguredStep } from "@/config/sourceSteps";
import type { DagRun, DagStep, DagStepProgress, SyncJob, SyncTask } from "@/api";
import { formatStamp } from "@/config/timeFormat";

/// What the pushed task board contributes to a row.
///
/// The board (`tasks` on a `GET /api/sync/stream` frame) speaks the
/// same per-step vocabulary as the runner's `current_state`, and
/// arrives up to 400 ms after the fact instead of on the next fetch of
/// the runner's record. So
/// it is not a new source of truth — it is the *same* one, earlier, and
/// is folded in as such rather than given its own precedence tier.
///
/// Only the live half is taken. A terminal board state carries neither
/// the timestamp nor the error the row has to show, and both live in
/// the runner's record — so a step going terminal is a signal to
/// refetch, never something to paint from here. `pushedOverlay` returns
/// what to overlay; `boardWentTerminal` says when to go and ask.
export type Overlay = {
  current_state: string | null;
  progress: DagStepProgress | null;
};

/// Board states that mean "this step is finished, one way or another".
/// Mirrors `task_state_for` in the backend's worker.
const TERMINAL_BOARD = new Set(["done", "skipped", "not_selected", "failed", "blocked"]);

/// Fold one pushed task board into per-step overlays.
///
/// `todo` deliberately produces nothing: a step the runner has not
/// reached is exactly the absent-`current_state` case the queued branch
/// of `stepStatus` already handles, and inventing a state for it here
/// would only add a second way to say the same thing.
export function pushedOverlay(
  tasks: SyncTask[],
  now: string,
): Record<string, Overlay> {
  const out: Record<string, Overlay> = {};
  for (const t of tasks) {
    if (t.state !== "running") continue;
    out[t.id] = {
      current_state: "running",
      // The board's `detail` is "<done>/<total> <msg>" — rendered for a
      // person, not parsed back. Carrying it as the message keeps the
      // tooltip live; the numeric bar stays with `/api/dag`, which has
      // the counts as numbers.
      progress: t.detail
        ? { done: null, total: null, msg: t.detail, updated_at: now }
        : null,
    };
  }
  return out;
}

/// Did any step on this board just reach a terminal state? That is when
/// the runner's record has something the board cannot supply.
export function boardWentTerminal(tasks: SyncTask[]): boolean {
  return tasks.some((t) => TERMINAL_BOARD.has(t.state));
}

/// Apply an overlay to what the last fetch of the runner's record knew
/// about a step.
///
/// Kept here rather than in the view so the fold is covered by the same
/// timeline tests as everything else it feeds.
export function withOverlay(
  step: DagStep | undefined,
  id: string,
  overlay: Overlay | undefined,
): DagStep | undefined {
  if (!overlay) return step;
  const base: DagStep = step ?? {
    id,
    command: "",
    inputs: [],
    outputs: [],
    deps: [],
    last_run: null,
    current_state: null,
    progress: null,
  };
  return {
    ...base,
    current_state: overlay.current_state ?? base.current_state,
    // The fetched progress has real numbers; the pushed one has only a
    // message. Prefer whichever is more informative rather than letting
    // the newer one erase a bar.
    progress: base.progress?.total != null ? base.progress : (overlay.progress ?? base.progress),
  };
}

/// The run a row should be judged against.
///
/// `/api/dag` is fetched, not pushed, so right after a sync starts it
/// may still describe the *previous* run — closed, and therefore
/// vetoing every live reading. The pushed board proves a run is in
/// flight before that fetch lands, and when it does, it wins: a closed
/// record cannot out-rank evidence of a step currently running.
///
/// The window is much shorter than it used to be — the fetch is
/// triggered by the record actually moving rather than by a 2-second
/// timer — but it is not zero, and this is what covers it.
export function effectiveRun(
  fetched: DagRun | null,
  liveJob: SyncJob | undefined,
): DagRun | null {
  if (!liveJob || liveJob.state !== "running") return fetched;
  if (fetched && !fetched.finished_at) return fetched;
  const started = liveJob.started_at ?? liveJob.created_at;
  return { run_id: started, started_at: started, finished_at: null, live: true };
}

/// One row's status, reduced to a vocabulary the Status column can draw.
///
/// `key` picks the glyph and the colour; `label` is the word it stands
/// for, and — because that column is icons — is the only place the word
/// still appears, as the tooltip and the accessible name. `detail` is
/// the sentence worth reading when there is one (a failure message, how
/// a run died, which job a queued row is waiting on).
export type StatusView = {
  key: string;
  label: string;
  /// When this status was reached. Feeds the "Last synced" column, so
  /// the two can never disagree about which run they describe.
  at: string | null;
  detail: string | null;
};

/// The word each status key stands for.
///
/// `skipped_up_to_date` is the runner's word; "Up to date" is what it
/// means to someone looking at a table.
export const STATUS_LABEL: Record<string, string> = {
  running: "Running",
  queued: "Queued",
  succeeded: "Succeeded",
  skipped_up_to_date: "Up to date",
  failed: "Failed",
  blocked: "Blocked",
  interrupted: "Interrupted",
  never_run: "Never run",
};

function view(key: string, at: string | null, detail: string | null): StatusView {
  return { key, label: STATUS_LABEL[key] ?? key.replace(/_/g, " "), at, detail };
}

/// "a", "a and b", "a, b and c". A tooltip is prose, and a row waiting
/// on two steps should read like a sentence rather than an array.
function listOf(items: string[]): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

/// Has this step already been reached by the run `job` started?
///
/// Both timestamps are this tree's ISO-8601-with-offset, so `Date`
/// parses them and the comparison is offset-correct without either side
/// being normalized first. A job with no `started_at` has not been
/// claimed by the worker yet, so nothing can have been reached.
///
/// Unparsable input answers "no": that leaves the row queued, which is
/// the reading that stays true for longest — the next fetch corrects it
/// either way.
function reachedSince(
  last: { started_at: string; finished_at: string | null } | null,
  job: { started_at: string | null },
): boolean {
  if (!last || !job.started_at) return false;
  const at = Date.parse(last.finished_at ?? last.started_at);
  const since = Date.parse(job.started_at);
  return Number.isFinite(at) && Number.isFinite(since) && at >= since;
}

/// step id → the ids of the steps that name it as an input. The DAG's
/// edges, read the direction the scheduler reads them.
export function dependentsOf(steps: ConfiguredStep[]): Record<string, string[]> {
  const m: Record<string, string[]> = {};
  for (const s of steps) {
    if (s.kind !== "step") continue;
    for (const input of s.inputs) (m[input] ??= []).push(s.id);
  }
  return m;
}

/// Every step a sync of `seeds` will consider: the seeds plus their
/// transitive dependents.
///
/// This mirrors `Runner::runnable_subgraph` — reachability in the
/// graph, and nothing about run-time state. It has to, because it is
/// what lets a row say "queued" the moment the button is pressed,
/// before the runner exists to be asked.
export function closureOf(
  dependents: Record<string, string[]>,
  seeds: string[],
): Set<string> {
  const seen = new Set<string>();
  const queue = [...seeds];
  while (queue.length) {
    const id = queue.shift()!;
    if (seen.has(id)) continue;
    seen.add(id);
    for (const d of dependents[id] ?? []) queue.push(d);
  }
  return seen;
}

/// The source steps a given step ultimately reads from: walk `inputs`
/// up until every branch reaches a step that declares none.
///
/// These are exactly the ids `--sync` accepts (`datalib-dag` rejects
/// anything else with "not a source step"), which is why they are worth
/// naming — a row that can't be run itself can say which rows carry it.
export function sourcesFeeding(steps: ConfiguredStep[], id: string): string[] {
  const byId = new Map(steps.map((s) => [s.id, s]));
  const found = new Set<string>();
  const seen = new Set<string>();
  const queue = [id];
  while (queue.length) {
    const at = queue.shift()!;
    if (seen.has(at)) continue;
    seen.add(at);
    const step = byId.get(at);
    // An input naming no step at all can't happen in a config the
    // loader accepted, but this runs against unsaved text too.
    if (!step) continue;
    if (step.inputs.length === 0) {
      if (at !== id) found.add(at);
      continue;
    }
    queue.push(...step.inputs);
  }
  return [...found].sort();
}

/// The steps a queued step is actually waiting behind: its declared
/// inputs that have not finished in the run now in flight.
///
/// "Queued" alone is a weak thing to tell someone — it says a row will
/// run without saying what it is behind. A render step waiting on its
/// download and a download waiting only for the worker to pick the job
/// up look identical in the column and are different situations, and
/// the difference is exactly what the DAG already knows.
///
/// Direct inputs only. The transitive set is the rest of the pipeline
/// and reads as noise; naming the one or two steps immediately ahead is
/// what answers the question.
export function waitingOn(
  steps: ConfiguredStep[],
  id: string,
  /// Has this step reached a terminal state in the current run? A step
  /// that is *running* has not, and is the most useful thing to name.
  isFinished: (stepId: string) => boolean,
): string[] {
  const step = steps.find((s) => s.id === id);
  if (!step) return [];
  return step.inputs.filter((input) => !isFinished(input)).sort();
}

/// step id → the queued-or-running job that has claimed it.
///
/// The job queue, not the runner's record, because this has to be true
/// during the window the runner does not yet exist: a click enqueues a
/// row, and the worker picks it up on its next poll. For that second or
/// two the runner's record still describes the *previous* run, and a
/// grid reading only that shows nothing happening — which is exactly
/// what pressing the play button used to look like.
export function claimedBy(
  steps: ConfiguredStep[],
  jobs: SyncJob[],
): Map<string, SyncJob> {
  const m = new Map<string, SyncJob>();
  const dependents = dependentsOf(steps);
  const stepIds = steps.filter((s) => s.kind === "step").map((s) => s.id);
  for (const job of jobs) {
    if (job.state !== "pending" && job.state !== "running") continue;
    // The worker splits `source_name` on commas and passes each as its
    // own `--sync`; empty means the whole config.
    const seeds = (job.source_name ?? "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    const scope = seeds.length ? closureOf(dependents, seeds) : new Set(stepIds);
    for (const id of scope) if (!m.has(id)) m.set(id, job);
  }
  return m;
}

/// What a step is doing right now, or did last.
///
/// Three sources, in this precedence, and the order is the whole point:
///
/// 1. **The runner's record for a run in flight.** Only `running` is
///    read from it — every terminal state it could report has already
///    been written to `last_run`, timestamps and all, so reading it
///    twice would only create a way for the two to disagree.
/// 2. **The job queue**, for a step a job has claimed but the runner
///    has not reached: `queued`.
/// 3. **`last_run`** — what this step last actually did.
///
/// `not_selected` appears nowhere. It is a fact about a *run* ("this
/// one didn't ask for me"), not about the step, and letting it reach a
/// row overwrote a real history with a non-event: a source that
/// succeeded yesterday came back as "not selected", stamped with the
/// time of a run that never touched it. The runner no longer records it
/// as a `last_run`, and `GET /api/dag` drops the ones already on disk;
/// this is the reader-side half of the same rule.
///
/// A run whose record has no `finished_at` and whose lock nobody holds
/// is a run that died. Its steps say `interrupted` rather than spinning
/// forever.
export function stepStatus(args: {
  /// The step being described. Passed separately from `step`, which is
  /// absent until a run has reached this row at least once.
  id: string;
  step: DagStep | undefined;
  run: DagRun | null;
  claim: SyncJob | undefined;
  /// The steps this one is queued behind, from `waitingOn`. Only read
  /// in the queued branch, where it turns "Queued" into a sentence that
  /// says what the row is behind.
  waitingOn?: string[];
}): StatusView {
  const { step, run, claim } = args;
  const last = step?.last_run ?? null;
  const runInFlight = run && !run.finished_at ? run : null;
  // Only a run still in flight says anything about now. A closed
  // record's `states` map is last run's history, and `last_run` tells
  // it better.
  const current = runInFlight ? (step?.current_state ?? null) : null;

  const died = (at: string) =>
    view(
      "interrupted",
      at,
      `Started ${formatStamp(at)} and never finished — no runner holds this root now, ` +
        `so it was killed or crashed.`,
    );

  if (current === "running") {
    const at = last?.started_at ?? runInFlight!.started_at;
    return runInFlight!.live ? view("running", at, null) : died(at);
  }

  // Claimed, and the runner hasn't reached it. `current` being set at
  // all means it has — including `not_selected`, which is the runner
  // saying this row is out of scope after all, and is more current than
  // the closure we predicted.
  //
  // The second guard is about *staleness*, not scope, and it is the
  // reason this can't be judged from the queue alone. The queue and the
  // runner's record are two independent fetches, so a snapshot can pair
  // a finished run with a job row that hasn't been marked done yet.
  // Without the guard the row flashes back to "Queued" after the sync
  // completes — a status going backwards, which is worse than a stale
  // one, because it reads as "it's about to run again".
  //
  // `last_run` at or after the job's start means this job already did
  // its work here, whatever the queue still says.
  if (claim && !current && !reachedSince(last, claim)) {
    // "the sync of pdfs/raw" reads badly on pdfs/raw's own row, which
    // is the row most likely to be read: a source step is what you
    // pressed the button on. Name the sync only when it is some *other*
    // row's.
    const seeds = (claim.source_name ?? "")
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean);
    const isOwnSync = seeds.length === 1 && seeds[0] === args.id;
    const sync = !claim.source_name
      ? "a sync of everything"
      : isOwnSync
        ? "this sync"
        : `the sync of ${claim.source_name}`;
    // Name what this row is behind. Upstream steps first, because that
    // is the specific answer; the job itself is the fallback when there
    // is nothing upstream left to wait for.
    const blockers = args.waitingOn ?? [];
    const detail = blockers.length
      ? `Waiting for ${listOf(blockers)} to finish, in ${sync}.`
      : claim.state === "pending"
        ? `Waiting for ${sync} to start.`
        : `Waiting its turn in ${sync}.`;
    return view("queued", last?.finished_at ?? last?.started_at ?? null, detail);
  }

  if (!last) return view("never_run", null, null);
  // An open record with no status is a step that was dispatched and
  // never finished — the run it belonged to is gone by now, or the
  // branch above would have caught it.
  if (!last.status) return died(last.started_at);
  return view(last.status, last.finished_at ?? last.started_at, last.error ?? null);
}
