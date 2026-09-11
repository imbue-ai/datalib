// What a pipeline row's Status column says, and why.

import type { ConfiguredStep } from "@/config/sourceSteps";
import type {
  DagRun,
  DagRunState,
  DagStep,
  DagStepProgress,
  Diagnostic,
  SyncJob,
  SyncTask,
  SyncTaskState,
} from "@/api";
import { formatStamp } from "@/config/timeFormat";

/// What the pushed task board contributes to a row.
///
/// Only the live half is taken: a terminal board state carries neither the
/// timestamp nor the error the row has to show, so a step going terminal is a
/// signal to refetch rather than something to paint from here.
export type Overlay = {
  current_state: DagRunState | null;
  progress: DagStepProgress | null;
};

/// Board states that mean "this step is finished, one way or another".
/// Mirrors `TaskState::is_terminal` in the backend's worker.
const TERMINAL_BOARD = new Set<SyncTaskState>([
  "done",
  "skipped",
  "not_selected",
  "failed",
  "blocked",
]);

/// The board's word for a step, back in the runner's vocabulary — the
/// inverse of `TaskState::for_run_state`. `todo` has no run state: it
/// means the scheduler has not reached the step.
const BOARD_TO_RUN_STATE: Record<SyncTaskState, DagRunState | null> = {
  todo: null,
  running: "running",
  done: "succeeded",
  skipped: "skipped_up_to_date",
  not_selected: "not_selected",
  failed: "failed",
  blocked: "blocked",
};

/// Fold one pushed task board into per-step overlays.
///
/// A step the board reports as **terminal** does produce one, and must.
/// `todo` means "not reached", which the queued branch already says; terminal
/// means "reached, and finished in *this* run", which nothing else on screen
/// knows until `/api/dag` catches up. Dropping it sent a row that had just
/// been Running back to Queued for the length of that fetch.
export function pushedOverlay(
  tasks: SyncTask[],
  now: string,
): Record<string, Overlay> {
  const out: Record<string, Overlay> = {};
  for (const t of tasks) {
    if (TERMINAL_BOARD.has(t.state)) {
      // No progress: a finished step's last progress line is not news,
      // and `withOverlay` keeps whatever the fetch had.
      out[t.id] = { current_state: BOARD_TO_RUN_STATE[t.state], progress: null };
      continue;
    }
    if (t.state !== "running") continue;
    out[t.id] = {
      current_state: "running",
      // The board's `detail` is "<done>/<total> <msg>" — rendered for a
      // person, not parsed back. Carrying it as the message keeps the
      // tooltip live; the numeric bar stays with `/api/dag`, which has
      // the counts as numbers.
      progress: t.detail ? { msg: t.detail, metrics: {}, updated_at: now } : null,
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
export function withOverlay(
  step: DagStep | undefined,
  id: string,
  overlay: Overlay | undefined,
  /// True when the fetched record describes a *previous* run, so its
  /// `current_state` is about different work and only the board can speak for
  /// now. `last_run` is deliberately untouched: that is this row's history, it
  /// is still correct, and blanking it would send the row to "Never run",
  /// which ranks below Queued and so is its own way of going backwards.
  baseStateIsStale = false,
): DagStep | undefined {
  if (!overlay && !baseStateIsStale) return step;
  if (!overlay && !step) return step;
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
    current_state:
      overlay?.current_state ?? (baseStateIsStale ? null : base.current_state),
    // The fetched progress has real numbers; the pushed one has only a
    // message. Prefer whichever is more informative rather than letting
    // the newer one erase a bar.
    progress:
      base.progress && Object.keys(base.progress.metrics).length > 0
        ? base.progress
        : (overlay?.progress ?? base.progress),
  };
}

/// The run a row should be judged against.
export type EffectiveRun = DagRun & {
  /// True when this run was **not** reported by the runner — it was
  /// inferred from the queue because a job is running and the fetched
  /// record has not caught up.
  ///
  /// Load-bearing, and not merely informational. A synthesized run says
  /// the runner has written nothing about *this* run yet, so every
  /// per-step `current_state` in the fetched record still belongs to
  /// the run before it. Reading one then is reading the wrong run's
  /// answer, which is exactly how a re-synced row painted the previous
  /// run's Succeeded between Queued and Running. `withOverlay` takes
  /// this flag and drops the stale state; the pushed board is the only
  /// source that can speak for a run this new.
  synthesized?: boolean;
};

export function effectiveRun(
  fetched: DagRun | null,
  liveJob: SyncJob | undefined,
): EffectiveRun | null {
  if (!liveJob || liveJob.state !== "running") return fetched;
  if (fetched && !fetched.finished_at) return fetched;
  const started = liveJob.started_at ?? liveJob.created_at;
  return {
    run_id: started,
    started_at: started,
    finished_at: null,
    live: true,
    synthesized: true,
  };
}

/// How far through a run each status is.
///
/// **`config_rejected` and `config_blocked` are deliberately absent,
/// and must stay absent.** They are not points in a run — they say the
/// entry is not in the pipeline at all, which is news that has to be
/// able to arrive *mid-run* and move a row backwards from `Succeeded`.
/// Ranking them would let the floor pin a row at a status describing a
/// config that no longer exists, which is the precise shape of the bug
/// this whole change is about. `statusFloor` passes an unranked status
/// through and forgets the row, which is what we want here: the next
/// status after a config edit starts from nothing.
///
/// "Never run" sits *below* Queued: it is the absence of history, so
/// seeing it after a sync was queued really is going backwards.
export const STATUS_RANK: Record<string, number> = {
  never_run: -1,
  queued: 0,
  running: 1,
  succeeded: 2,
  skipped_up_to_date: 2,
  failed: 2,
  blocked: 2,
  interrupted: 2,
};

/// A floor under a row's status, holding it to the furthest it has got
/// within one run.
export function statusFloor(): (id: string, run: string, next: StatusView) => StatusView {
  const seen = new Map<string, { run: string; view: StatusView }>();
  return (id, run, next) => {
    const prev = seen.get(id);
    const nextRank = STATUS_RANK[next.key];
    // A status outside the vocabulary is passed through and forgotten.
    // Holding a row at a rank we cannot compare would be worse than the
    // flicker this exists to stop.
    if (nextRank === undefined) {
      seen.delete(id);
      return next;
    }
    if (prev && prev.run === run && STATUS_RANK[prev.view.key] > nextRank) {
      return prev.view;
    }
    seen.set(id, { run, view: next });
    return next;
  };
}

/// One row's status, reduced to a vocabulary the Status column can draw.
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
  // The two the config loader produces. A row in either is in the
  // config file and *not* in the pipeline, which is why neither
  // borrows a word from the runner's vocabulary: "Failed" would claim
  // it ran, and "Blocked" already means an upstream step failed.
  config_rejected: "Not loaded",
  config_blocked: "Can\u2019t run",
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
export function claimedBy(
  steps: ConfiguredStep[],
  jobs: SyncJob[],
): Map<string, SyncJob> {
  const m = new Map<string, SyncJob>();
  const dependents = dependentsOf(steps);
  const stepIds = steps.filter((s) => s.kind === "step").map((s) => s.id);
  for (const job of jobs) {
    if (job.state !== "pending" && job.state !== "running") continue;
    // The worker splits `source_ids` on commas and passes each as its
    // own `--sync`; empty means the whole config.
    const seeds = (job.source_ids ?? "")
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
/// `not_selected` appears nowhere. It is a fact about a *run* ("this
/// one didn't ask for me"), not about the step, and letting it reach a
/// row overwrote a real history with a non-event: a source that
/// succeeded yesterday came back as "not selected", stamped with the
/// time of a run that never touched it. The runner no longer records it
/// as a `last_run`, and `GET /api/dag` drops the ones already on disk;
/// this is the reader-side half of the same rule.
export function stepStatus(args: {
  /// The step being described. Passed separately from `step`, which is
  /// absent until a run has reached this row at least once.
  id: string;
  step: DagStep | undefined;
  run: EffectiveRun | null;
  claim: SyncJob | undefined;
  /// The steps this one is queued behind, from `waitingOn`. Only read
  /// in the queued branch, where it turns "Queued" into a sentence that
  /// says what the row is behind.
  waitingOn?: string[];
  /// The loader's reason this entry is not in the pipeline, if it
  /// isn't. Outranks every other source — see below.
  dropped?: Diagnostic | null;
}): StatusView {
  const { step, run, claim } = args;

  // A step the config loader dropped is not going to run, whatever the
  // runner's record still remembers about the last time it did. This
  // has to outrank everything else: a row still reading "Up to date"
  // from last week, for an entry that is no longer in the graph, is
  // exactly the failure #209 is about — the table looks healthy and the
  // data silently stops moving. `at` is null for the same reason, since
  // the timestamp would describe a run this config never took part in.
  if (args.dropped) {
    const d = args.dropped;
    const key = d.severity === "blocked" ? "config_blocked" : "config_rejected";
    return view(key, null, d.help ? `${d.message} \u2014 ${d.help}` : d.message);
  }
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
  if (claim && !current && !reachedSince(last, claim)) {
    // "the sync of pdfs/raw" reads badly on pdfs/raw's own row, which
    // is the row most likely to be read: a source step is what you
    // pressed the button on. Name the sync only when it is some *other*
    // row's.
    const seeds = (claim.source_ids ?? "")
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean);
    const isOwnSync = seeds.length === 1 && seeds[0] === args.id;
    const sync = !claim.source_ids
      ? "a sync of everything"
      : isOwnSync
        ? "this sync"
        : `the sync of ${claim.source_ids}`;
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
