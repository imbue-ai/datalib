// The pipeline Status column's state machine.
//
// Two kinds of test here, and the second is the one that matters.
//
// **Snapshot cases** pin what a single (queue, runner-record) pair
// means. Necessary, but weak: every wrong answer this file has produced
// was legal at some instant and wrong as part of a sequence.
//
// **Timeline cases** replay an ordered series of snapshots — the ones a
// polling grid actually sees across a sync — and assert properties of
// the whole run: that the row reaches Running, that it ends on the real
// outcome, and above all that it never goes *backwards*. That last one
// is the property a snapshot test cannot express, and it is the one
// that catches the interesting bugs: the queue and the runner's record
// arrive from two independent fetches, so any pairing of a fresh one
// with a stale one is a snapshot the UI will really be handed.

import { describe, expect, it } from "vitest";
import {
  boardWentTerminal,
  claimedBy,
  effectiveRun,
  pushedOverlay,
  sourcesFeeding,
  statusFloor,
  stepStatus,
  STATUS_LABEL,
  STATUS_RANK,
  waitingOn,
  withOverlay,
  type Overlay,
  type StatusView,
} from "./pipelineStatus";
import type { ConfiguredStep } from "./sourceSteps";
import type { DagRun, DagStep, SyncJob, SyncTask } from "@/api";

/// A two-source graph with a shared fan-in, which is the shape every
/// real config has: `a/raw → a/rendered_md → unified_index/grid`, and
/// the same for `b`.
function steps(): ConfiguredStep[] {
  const mk = (id: string, inputs: string[]): ConfiguredStep => ({
    id,
    kind: "step",
    name: id,
    phase: "other",
    type: null,
    inputs,
    params: {},
    start: 0,
    end: 0,
  });
  return [
    mk("a/raw", []),
    mk("a/rendered_md", ["a/raw"]),
    mk("b/raw", []),
    mk("b/rendered_md", ["b/raw"]),
    mk("unified_index/grid", ["a/rendered_md", "b/rendered_md"]),
  ];
}

const T = {
  jobStart: "2026-09-01T10:00:00+02:00",
  runStart: "2026-09-01T10:00:01+02:00",
  aDone: "2026-09-01T10:00:09+02:00",
  runEnd: "2026-09-01T10:00:20+02:00",
  yesterday: "2026-08-31T09:00:00+02:00",
};

function job(over: Partial<SyncJob> = {}): SyncJob {
  return {
    id: "job-1",
    kind: "all",
    source_name: "a/raw",
    state: "running",
    progress_pct: null,
    progress_msg: null,
    error: null,
    created_at: T.jobStart,
    started_at: T.jobStart,
    finished_at: null,
    ...over,
  };
}

function dagStep(over: Partial<DagStep> = {}): DagStep {
  return {
    id: "a/raw",
    command: "",
    inputs: [],
    outputs: [],
    deps: [],
    last_run: null,
    current_state: null,
    progress: null,
    ...over,
  };
}

const liveRun: DagRun = {
  run_id: T.runStart,
  started_at: T.runStart,
  finished_at: null,
  live: true,
};

/// One frame of what the grid holds: the queue and the runner's record,
/// exactly the two things it polls.
type Frame = { jobs: SyncJob[]; run: DagRun | null; dag: Record<string, DagStep> };

function statusIn(frame: Frame, id: string): StatusView {
  return stepStatus({
    id,
    step: frame.dag[id],
    run: frame.run,
    claim: claimedBy(steps(), frame.jobs).get(id),
  });
}

describe("what a single snapshot means", () => {
  it("a step nothing has ever touched has never run", () => {
    const s = statusIn({ jobs: [], run: null, dag: {} }, "a/raw");
    expect(s.key).toBe("never_run");
    expect(s.at).toBeNull();
  });

  it("a pending job queues the step it names and everything downstream", () => {
    // Nothing is running yet — the worker has not even claimed the job.
    // This is the window that used to show nothing at all.
    const claims = claimedBy(steps(), [job({ state: "pending", started_at: null })]);
    expect([...claims.keys()].sort()).toEqual([
      "a/raw",
      "a/rendered_md",
      "unified_index/grid",
    ]);
    // ...and not the other source's chain, which this sync never reaches.
    expect(claims.has("b/raw")).toBe(false);
    expect(claims.has("b/rendered_md")).toBe(false);
  });

  it("a job naming no source claims every step", () => {
    const claims = claimedBy(steps(), [job({ source_name: null })]);
    expect(claims.size).toBe(5);
  });

  it("a run in flight with a dead runner reads as interrupted, not running", () => {
    const s = statusIn(
      {
        jobs: [],
        run: { ...liveRun, live: false },
        dag: { "a/raw": dagStep({ current_state: "running" }) },
      },
      "a/raw",
    );
    expect(s.key).toBe("interrupted");
    expect(s.detail).toContain("killed or crashed");
  });

  it("a not_selected current state falls through to what the step last did", () => {
    // The runner walks every step to publish output versions, so a
    // subset sync reaches this one and reports it out of scope. That is
    // a fact about the run; the row goes on showing the step's own
    // history.
    const s = statusIn(
      {
        jobs: [],
        run: liveRun,
        dag: {
          "b/raw": dagStep({
            id: "b/raw",
            current_state: "not_selected",
            last_run: {
              started_at: T.yesterday,
              finished_at: T.yesterday,
              status: "succeeded",
              attempts: 1,
              error: null,
            },
          }),
        },
      },
      "b/raw",
    );
    expect(s.key).toBe("succeeded");
    expect(s.at).toBe(T.yesterday);
  });
});

describe("what a step can be run from", () => {
  it("names the source steps a fan-in would be carried by", () => {
    expect(sourcesFeeding(steps(), "unified_index/grid")).toEqual(["a/raw", "b/raw"]);
    expect(sourcesFeeding(steps(), "a/rendered_md")).toEqual(["a/raw"]);
    // A source step is not fed by anything, including itself.
    expect(sourcesFeeding(steps(), "a/raw")).toEqual([]);
  });
});

// The sequence of frames a grid really sees across one "Sync a/raw",
// including the two places the queue and the runner's record disagree
// because they are fetched separately.
//
// Written as data so the properties below can be asserted over the
// whole run rather than at one instant.
const TIMELINE: { note: string; frame: Frame }[] = [
  {
    note: "clicked: the job exists, the worker has not claimed it, and the runner's record is still last run's",
    frame: {
      jobs: [job({ state: "pending", started_at: null })],
      run: { ...liveRun, run_id: "older", started_at: T.yesterday, finished_at: T.yesterday, live: false },
      dag: {},
    },
  },
  {
    note: "worker claimed the job; the runner has opened a record but reached nothing",
    frame: { jobs: [job()], run: liveRun, dag: {} },
  },
  {
    note: "the step is dispatched and running",
    frame: {
      jobs: [job()],
      run: liveRun,
      dag: {
        "a/raw": dagStep({
          current_state: "running",
          last_run: { started_at: T.runStart, finished_at: null, status: "", attempts: 0, error: null },
          progress: { done: 3, total: 10, msg: "page 3", updated_at: T.runStart },
        }),
      },
    },
  },
  {
    note: "the step finished, the run is still going (the fan-in is left)",
    frame: {
      jobs: [job()],
      run: liveRun,
      dag: {
        "a/raw": dagStep({
          current_state: "succeeded",
          last_run: { started_at: T.runStart, finished_at: T.aDone, status: "succeeded", attempts: 1, error: null },
        }),
      },
    },
  },
  {
    note: "the run closed, but the queue fetch is one poll behind and still says running",
    frame: {
      jobs: [job()],
      run: { ...liveRun, finished_at: T.runEnd, live: false },
      dag: {
        "a/raw": dagStep({
          current_state: "succeeded",
          last_run: { started_at: T.runStart, finished_at: T.aDone, status: "succeeded", attempts: 1, error: null },
        }),
      },
    },
  },
  {
    note: "settled: the queue has caught up",
    frame: {
      jobs: [job({ state: "done", finished_at: T.runEnd })],
      run: { ...liveRun, finished_at: T.runEnd, live: false },
      dag: {
        "a/raw": dagStep({
          current_state: "succeeded",
          last_run: { started_at: T.runStart, finished_at: T.aDone, status: "succeeded", attempts: 1, error: null },
        }),
      },
    },
  },
];

describe("the sequence a sync actually produces", () => {
  const seen = TIMELINE.map((t) => ({ note: t.note, s: statusIn(t.frame, "a/raw") }));

  it("shows something is happening from the very first frame", () => {
    // The whole complaint: pressing the button and seeing nothing. The
    // first frame is before the worker has even claimed the job, and it
    // still has to say so.
    expect(seen[0].s.key).toBe("queued");
    expect(seen[1].s.key).toBe("queued");
  });

  it("reaches Running, with the step's own progress", () => {
    expect(seen[2].s.key).toBe("running");
    expect(TIMELINE[2].frame.dag["a/raw"].progress).toMatchObject({ done: 3, total: 10 });
  });

  it("ends on the real outcome, with the time it actually happened", () => {
    const last = seen[seen.length - 1].s;
    expect(last.key).toBe("succeeded");
    expect(last.at).toBe(T.aDone);
  });

  it("never goes backwards — including when the queue lags the runner", () => {
    // The property a snapshot test cannot state. Frame 4 pairs a closed
    // run with a job row still marked running; reading the queue alone
    // there sends a finished step back to "Queued", which reads as
    // "about to run again".
    const rank: Record<string, number> = { queued: 0, running: 1, succeeded: 2, failed: 2 };
    const ranks = seen.map((x) => rank[x.s.key]);
    for (let i = 1; i < ranks.length; i++) {
      expect(
        ranks[i],
        `went backwards at frame ${i} (${seen[i].note}): ` +
          `${seen[i - 1].s.key} -> ${seen[i].s.key}`,
      ).toBeGreaterThanOrEqual(ranks[i - 1]);
    }
  });

  it("holds the Stop button for exactly as long as the job is live", () => {
    // The button's face is `claimedBy`, so this is the same question as
    // "is there work outstanding for this row". It must not linger once
    // the queue settles, or the row can never be run again.
    const claimed = TIMELINE.map((t) => claimedBy(steps(), t.frame.jobs).has("a/raw"));
    expect(claimed).toEqual([true, true, true, true, true, false]);
  });

  it("leaves the other source's chain alone throughout", () => {
    // The regression behind "Last synced moved on a sync of something
    // else": b's rows are outside this sync, and nothing in the whole
    // sequence may give them a status or a timestamp.
    for (const { note, frame } of TIMELINE) {
      const s = statusIn(frame, "b/raw");
      expect(s.key, `b/raw changed at: ${note}`).toBe("never_run");
      expect(s.at, `b/raw got a timestamp at: ${note}`).toBeNull();
    }
  });
});

// The pushed sequence: what arrives over `GET /api/sync/stream`, in the
// order the worker sends it, with NO `/api/dag` poll landing in
// between.
//
// This is the case the grid is judged on, because it is the fast path
// and the one a person actually watches. The runner's record is
// deliberately left stale throughout — describing the *previous* run,
// closed — so that any reading which lets a polled record veto a pushed
// one shows up here as a row stuck on "Queued".
describe("the pushed sequence, with the polled record still stale", () => {
  // Last run's record: closed, and about a different run entirely.
  const stalePoll: DagRun = {
    run_id: "yesterday",
    started_at: T.yesterday,
    finished_at: T.yesterday,
    live: false,
  };

  const frames: { note: string; state: SyncJob["state"]; tasks: SyncTask[] }[] = [
    {
      note: "enqueue handler publishes the moment the row is written",
      state: "pending",
      tasks: [],
    },
    {
      note: "worker claimed it; the plan is known, nothing dispatched",
      state: "running",
      tasks: [
        { id: "a/raw", state: "todo" },
        { id: "a/rendered_md", state: "todo" },
      ],
    },
    {
      note: "the step is dispatched — this is the frame that has to say Running",
      state: "running",
      tasks: [
        { id: "a/raw", state: "running", detail: "3/10 page 3" },
        { id: "a/rendered_md", state: "todo" },
      ],
    },
    {
      note: "further into the same step",
      state: "running",
      tasks: [
        { id: "a/raw", state: "running", detail: "7/10 page 7" },
        { id: "a/rendered_md", state: "todo" },
      ],
    },
  ];

  /// Fold one pushed frame the way `onJobEvent` does, then read the row.
  function readPushed(frame: (typeof frames)[number], id: string) {
    const active = frame.state === "pending" || frame.state === "running";
    const overlay = active ? pushedOverlay(frame.tasks, T.runStart) : {};
    const j = job({ state: frame.state, started_at: active ? T.jobStart : null });
    return stepStatus({
      id,
      step: withOverlay(undefined, id, overlay[id]),
      run: effectiveRun(stalePoll, frame.state === "running" ? j : undefined),
      claim: claimedBy(steps(), [j]).get(id),
    });
  }

  it("says Queued from the enqueue frame — before any runner exists", () => {
    expect(readPushed(frames[0], "a/raw").key).toBe("queued");
  });

  it("says Queued while the plan is known but nothing is dispatched", () => {
    expect(readPushed(frames[1], "a/raw").key).toBe("queued");
  });

  it("says Running on the dispatch frame, without waiting for a poll", () => {
    // The property that makes this push rather than poll. A stale
    // polled record must not be able to veto it: `effectiveRun` is what
    // stops a closed `finished_at` from forcing this back to Queued.
    const s = readPushed(frames[2], "a/raw");
    expect(s.key).toBe("running");
  });

  it("carries the step's own words while it runs", () => {
    const overlay = pushedOverlay(frames[2].tasks, T.runStart);
    expect(overlay["a/raw"].progress?.msg).toBe("3/10 page 3");
  });

  it("never goes backwards across the pushed sequence either", () => {
    const rank: Record<string, number> = { queued: 0, running: 1, succeeded: 2 };
    const seq = frames.map((f) => readPushed(f, "a/raw").key);
    expect(seq).toEqual(["queued", "queued", "running", "running"]);
    for (let i = 1; i < seq.length; i++) {
      expect(rank[seq[i]]).toBeGreaterThanOrEqual(rank[seq[i - 1]]);
    }
  });

  it("leaves the downstream step queued, not running", () => {
    // One step running does not make its consumer running. The board
    // says `todo` for it, which must produce no overlay at all.
    expect(readPushed(frames[2], "a/rendered_md").key).toBe("queued");
  });

  it("asks the runner's record only when a step goes terminal", () => {
    // The board cannot supply a finish time or an error, so those are
    // the transitions that cost a fetch — and the only ones.
    expect(boardWentTerminal(frames[1].tasks)).toBe(false);
    expect(boardWentTerminal(frames[2].tasks)).toBe(false);
    expect(boardWentTerminal([{ id: "a/raw", state: "done" }])).toBe(true);
    expect(boardWentTerminal([{ id: "a/raw", state: "failed" }])).toBe(true);
  });

  it("does not invent a live run when nothing is running", () => {
    // `effectiveRun` may only override a stale record on evidence. With
    // no running job there is none, and the polled answer stands.
    expect(effectiveRun(stalePoll, undefined)).toBe(stalePoll);
    expect(effectiveRun(stalePoll, job({ state: "pending" }))).toBe(stalePoll);
  });

  it("keeps a polled numeric bar rather than letting a pushed message erase it", () => {
    const polled = dagStep({
      current_state: "running",
      progress: { done: 5, total: 10, msg: "polled", updated_at: T.runStart },
    });
    const merged = withOverlay(polled, "a/raw", {
      current_state: "running",
      progress: { done: null, total: null, msg: "pushed", updated_at: T.runStart },
    });
    expect(merged?.progress).toMatchObject({ done: 5, total: 10 });
  });
});

// "Queued" on its own says a row will run without saying what it is
// behind — and a render step waiting on its download is a different
// situation from a download waiting only for the worker to pick the job
// up. The DAG already knows the difference; these pin that it is said.
describe("what a queued row is waiting for", () => {
  const finishedNone = () => false;
  const finishedAll = () => true;

  it("names the direct inputs that have not finished", () => {
    expect(waitingOn(steps(), "unified_index/grid", finishedNone)).toEqual([
      "a/rendered_md",
      "b/rendered_md",
    ]);
    // Only the direct ones. `a/raw` is upstream too, but naming the
    // whole transitive set is the rest of the pipeline.
    expect(waitingOn(steps(), "a/rendered_md", finishedNone)).toEqual(["a/raw"]);
  });

  it("drops inputs that already finished this run", () => {
    const done = (id: string) => id === "a/rendered_md";
    expect(waitingOn(steps(), "unified_index/grid", done)).toEqual(["b/rendered_md"]);
    expect(waitingOn(steps(), "unified_index/grid", finishedAll)).toEqual([]);
  });

  it("a source step waits on nothing upstream", () => {
    expect(waitingOn(steps(), "a/raw", finishedNone)).toEqual([]);
  });

  function queuedDetail(id: string, blockers: string[], state: SyncJob["state"]) {
    const j = job({ state, source_name: "a/raw", started_at: state === "pending" ? null : T.jobStart });
    return stepStatus({
      id,
      step: undefined,
      run: null,
      claim: claimedBy(steps(), [j]).get(id),
      waitingOn: blockers,
    }).detail;
  }

  it("says what it is behind when something upstream is outstanding", () => {
    expect(queuedDetail("a/rendered_md", ["a/raw"], "running")).toBe(
      "Waiting for a/raw to finish, in the sync of a/raw.",
    );
  });

  it("reads as a sentence with more than one blocker", () => {
    expect(
      queuedDetail("unified_index/grid", ["a/rendered_md", "b/rendered_md"], "running"),
    ).toBe(
      "Waiting for a/rendered_md and b/rendered_md to finish, in the sync of a/raw.",
    );
  });

  it("distinguishes a job not yet started from one already going", () => {
    // Nothing upstream to name, so the job itself is the answer — and
    // "hasn't started" and "started, not my turn yet" are different
    // things to be told.
    // On `a/raw`'s own row "the sync of a/raw" is a mouthful that says
    // nothing — it *is* that row. Named only when it is someone else's.
    expect(queuedDetail("a/raw", [], "pending")).toBe("Waiting for this sync to start.");
    expect(queuedDetail("a/raw", [], "running")).toBe("Waiting its turn in this sync.");
  });
});

describe("a second sync of a row that has already run", () => {
  // The frame this exists for is the one `manager2-sync.spec.ts` caught
  // intermittently as
  // `went backwards: ["Queued","Succeeded","Running","Succeeded"]`.
  //
  // It only happens on a *re-*sync, which is why every timeline above
  // missed it: they all start from a row with no history. Press Sync on
  // a row that already succeeded once and, for as long as it takes
  // `/api/dag` to catch up, three things are true at the same time —
  // the queue says a job is running, the fetched record still describes
  // the *previous* run (closed), and that record's per-step
  // `current_state` is the previous run's terminal state.
  //
  // `effectiveRun` correctly synthesizes a live run from the queue, so
  // the row is judged against a run in flight. But the `current_state`
  // it then reads belongs to the run before. Being set at all was taken
  // as "the runner has reached this step", the queued branch was
  // skipped, and the row painted the *previous* run's Succeeded.
  const PREVIOUS: DagRun = {
    run_id: T.yesterday,
    started_at: T.yesterday,
    finished_at: T.yesterday,
    live: false,
  };
  const alreadySucceeded = dagStep({
    // The previous run's leftovers, both of them.
    current_state: "succeeded",
    last_run: {
      started_at: T.yesterday,
      finished_at: T.yesterday,
      status: "succeeded",
      attempts: 1,
      error: null,
    },
  });

  /// The grid's own composition, which is what the view does per row:
  /// pick the run to judge against, then fold the pushed board over the
  /// fetched record.
  function paint(jobs: SyncJob[], overlay: Record<string, Overlay>): StatusView {
    const live = jobs.find((j) => j.state === "running");
    const run = effectiveRun(PREVIOUS, live);
    return stepStatus({
      id: "a/raw",
      step: withOverlay(alreadySucceeded, "a/raw", overlay["a/raw"], !!run?.synthesized),
      run,
      claim: claimedBy(steps(), jobs).get("a/raw"),
    });
  }

  it("is queued the moment the job is, not still showing the last run", () => {
    expect(paint([job({ state: "pending", started_at: null })], {}).key).toBe("queued");
  });

  it("stays queued once the worker starts it, before the record catches up", () => {
    // The failing frame. The job is running; `/api/dag` has not been
    // rewritten yet, so everything it says is about yesterday.
    expect(paint([job({ state: "running" })], {}).key).toBe("queued");
  });

  it("reaches Running on the pushed board, without waiting for the fetch", () => {
    const overlay = pushedOverlay([{ id: "a/raw", state: "running" }], T.jobStart);
    expect(paint([job({ state: "running" })], overlay).key).toBe("running");
  });

  it("does not fall back to Queued when the board says it finished", () => {
    // The second backwards frame, found by repeating the e2e run: the
    // board drops a step from `running` the moment it is done, so
    // between that board and `/api/dag` catching up there was no
    // evidence left that the step had ever been reached — and the row
    // went back to Queued.
    const done = pushedOverlay([{ id: "a/raw", state: "done" }], T.jobStart);
    expect(paint([job({ state: "running" })], done).key).not.toBe("queued");
  });

  it("never goes backwards across the whole re-sync", () => {
    const board = pushedOverlay([{ id: "a/raw", state: "running" }], T.jobStart);
    const finished = pushedOverlay([{ id: "a/raw", state: "done" }], T.jobStart);
    const seen = [
      paint([job({ state: "pending", started_at: null })], {}),
      paint([job({ state: "running" })], {}),
      paint([job({ state: "running" })], board),
      // The board moves on before the fetch does.
      paint([job({ state: "running" })], finished),
      // The record finally lands, describing the run that just ended.
      stepStatus({
        id: "a/raw",
        step: dagStep({
          current_state: "succeeded",
          last_run: {
            started_at: T.runStart,
            finished_at: T.aDone,
            status: "succeeded",
            attempts: 1,
            error: null,
          },
        }),
        run: { ...liveRun, finished_at: T.runEnd, live: false },
        claim: undefined,
      }),
    ].map((s) => s.key);

    const rank: Record<string, number> = { never_run: -1, queued: 0, running: 1, succeeded: 2 };
    for (let i = 1; i < seen.length; i++) {
      expect(
        rank[seen[i]],
        `went backwards at frame ${i}: ${JSON.stringify(seen)}`,
      ).toBeGreaterThanOrEqual(rank[seen[i - 1]]);
    }
    expect(seen[seen.length - 1]).toBe("succeeded");
  });
});

describe("the floor under a row's status within one run", () => {
  // `stepStatus` describes one snapshot, and the snapshot is genuinely
  // ambiguous when neither the board nor the record says anything about
  // this step in the run now in flight. Both readings of that silence
  // shipped as bugs — see `statusFloor`. This is the missing input.
  const view = (key: string): StatusView => ({ key, label: key, at: null, detail: null });

  it("holds a row that has been Running against a lapse back to Queued", () => {
    const hold = statusFloor();
    expect(hold("a/raw", "job-1", view("queued")).key).toBe("queued");
    expect(hold("a/raw", "job-1", view("running")).key).toBe("running");
    // The frame the e2e caught: claim still running, no `current_state`
    // from either source, so `stepStatus` reads it as Queued.
    expect(hold("a/raw", "job-1", view("queued")).key).toBe("running");
    expect(hold("a/raw", "job-1", view("succeeded")).key).toBe("succeeded");
  });

  it("lets the next sync of the same row start at Queued again", () => {
    const hold = statusFloor();
    hold("a/raw", "job-1", view("running"));
    hold("a/raw", "job-1", view("succeeded"));
    // A new job is a new key, so the floor lifts. Without this a row
    // could never be seen queued twice.
    expect(hold("a/raw", "job-2", view("queued")).key).toBe("queued");
  });

  it("keeps rows apart", () => {
    const hold = statusFloor();
    hold("a/raw", "job-1", view("running"));
    expect(hold("b/raw", "job-1", view("queued")).key).toBe("queued");
  });

  it("returns the held view whole, not just its rank", () => {
    // The tooltip and the timestamp have to travel with the status, or
    // a held frame would describe itself with the wrong sentence.
    const hold = statusFloor();
    const running: StatusView = {
      key: "running",
      label: "Running",
      at: T.runStart,
      detail: "downloading 3/10",
    };
    hold("a/raw", "job-1", running);
    expect(hold("a/raw", "job-1", view("queued"))).toEqual(running);
  });

  it("passes through a status it cannot rank rather than freezing on it", () => {
    // A vocabulary this table has not met is exactly when holding a row
    // would be worst: we would be pinning it at a rank we invented.
    const hold = statusFloor();
    hold("a/raw", "job-1", view("running"));
    expect(hold("a/raw", "job-1", view("something_new")).key).toBe("something_new");
    // ...and having forgotten it, the next real status is free too.
    expect(hold("a/raw", "job-1", view("queued")).key).toBe("queued");
  });

  it("ranks every status the column can draw", () => {
    // Total on purpose: an unranked status silently opts out of the
    // floor, which is the one gap that would go unnoticed.
    for (const key of Object.keys(STATUS_LABEL)) {
      expect(STATUS_RANK[key], `${key} has no rank`).toBeDefined();
    }
  });
});
