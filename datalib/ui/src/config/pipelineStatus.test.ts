// The pipeline Status column's state machine.
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
  claimedBy,
  effectiveRun,
  sourcesFeeding,
  statusFloor,
  stepStatus,
  STATUS_LABEL,
  STATUS_RANK,
  waitingOn,
  stepForRun,
  type StatusView,
} from "./pipelineStatus";
import type { ConfiguredStep } from "./sourceSteps";
import type { DagRun, DagStep, SyncJob } from "@/api";

/// A two-source graph with a shared fan-in, which is the shape every
/// real config has: `a/ingest → a/render_markdown → unified_index/grid`, and
/// the same for `b`.
function steps(): ConfiguredStep[] {
  const mk = (id: string, inputs: string[]): ConfiguredStep => ({
    id,
    kind: "step",
    group: null,
    function: null,
    name: id,
    phase: "other",
    type: null,
    inputs,
    params: {},
    start: 0,
    end: 0,
  });
  return [
    mk("a/ingest", []),
    mk("a/render_markdown", ["a/ingest"]),
    mk("b/ingest", []),
    mk("b/render_markdown", ["b/ingest"]),
    mk("unified_index/grid_index", ["a/render_markdown", "b/render_markdown"]),
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
    source_ids: "a/ingest",
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
    id: "a/ingest",
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
    const s = statusIn({ jobs: [], run: null, dag: {} }, "a/ingest");
    expect(s.key).toBe("never_run");
    expect(s.at).toBeNull();
  });

  it("a pending job queues the step it names and everything downstream", () => {
    // Nothing is running yet — the worker has not even claimed the job.
    // This is the window that used to show nothing at all.
    const claims = claimedBy(steps(), [job({ state: "pending", started_at: null })]);
    expect([...claims.keys()].sort()).toEqual([
      "a/ingest",
      "a/render_markdown",
      "unified_index/grid_index",
    ]);
    // ...and not the other source's chain, which this sync never reaches.
    expect(claims.has("b/ingest")).toBe(false);
    expect(claims.has("b/render_markdown")).toBe(false);
  });

  it("a job naming no source claims every step", () => {
    const claims = claimedBy(steps(), [job({ source_ids: null })]);
    expect(claims.size).toBe(5);
  });

  it("a run in flight with a dead runner reads as interrupted, not running", () => {
    const s = statusIn(
      {
        jobs: [],
        run: { ...liveRun, live: false },
        dag: { "a/ingest": dagStep({ current_state: "running" }) },
      },
      "a/ingest",
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
          "b/ingest": dagStep({
            id: "b/ingest",
            current_state: "not_selected",
            last_run: {
              run_id: "r",
              started_at: T.yesterday,
              finished_at: T.yesterday,
              status: "succeeded",
              attempts: 1,
              error: null,
            },
          }),
        },
      },
      "b/ingest",
    );
    expect(s.key).toBe("succeeded");
    expect(s.at).toBe(T.yesterday);
  });
});

describe("what a step can be run from", () => {
  it("names the source steps a fan-in would be carried by", () => {
    expect(sourcesFeeding(steps(), "unified_index/grid_index")).toEqual(["a/ingest", "b/ingest"]);
    expect(sourcesFeeding(steps(), "a/render_markdown")).toEqual(["a/ingest"]);
    // A source step is not fed by anything, including itself.
    expect(sourcesFeeding(steps(), "a/ingest")).toEqual([]);
  });
});

// The sequence of frames a grid really sees across one "Sync a/ingest",
// including the two places the queue and the runner's record disagree
// because they are fetched separately.
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
        "a/ingest": dagStep({
          current_state: "running",
          last_run: { run_id: "r", started_at: T.runStart, finished_at: null, status: "", attempts: 0, error: null },
          progress: { msg: "page 3", metrics: { done: 3, queued: 7 }, errors: 0, updated_at: T.runStart },
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
        "a/ingest": dagStep({
          current_state: "succeeded",
          last_run: { run_id: "r", started_at: T.runStart, finished_at: T.aDone, status: "succeeded", attempts: 1, error: null },
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
        "a/ingest": dagStep({
          current_state: "succeeded",
          last_run: { run_id: "r", started_at: T.runStart, finished_at: T.aDone, status: "succeeded", attempts: 1, error: null },
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
        "a/ingest": dagStep({
          current_state: "succeeded",
          last_run: { run_id: "r", started_at: T.runStart, finished_at: T.aDone, status: "succeeded", attempts: 1, error: null },
        }),
      },
    },
  },
];

describe("the sequence a sync actually produces", () => {
  const seen = TIMELINE.map((t) => ({ note: t.note, s: statusIn(t.frame, "a/ingest") }));

  it("shows something is happening from the very first frame", () => {
    // The whole complaint: pressing the button and seeing nothing. The
    // first frame is before the worker has even claimed the job, and it
    // still has to say so.
    expect(seen[0].s.key).toBe("queued");
    expect(seen[1].s.key).toBe("queued");
  });

  it("reaches Running, with the step's own progress", () => {
    expect(seen[2].s.key).toBe("running");
    expect(TIMELINE[2].frame.dag["a/ingest"].progress?.metrics).toMatchObject({ done: 3, queued: 7 });
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
    const claimed = TIMELINE.map((t) => claimedBy(steps(), t.frame.jobs).has("a/ingest"));
    expect(claimed).toEqual([true, true, true, true, true, false]);
  });

  it("leaves the other source's chain alone throughout", () => {
    // The regression behind "Last synced moved on a sync of something
    // else": b's rows are outside this sync, and nothing in the whole
    // sequence may give them a status or a timestamp.
    for (const { note, frame } of TIMELINE) {
      const s = statusIn(frame, "b/ingest");
      expect(s.key, `b/ingest changed at: ${note}`).toBe("never_run");
      expect(s.at, `b/ingest got a timestamp at: ${note}`).toBeNull();
    }
  });
});

// The live sequence: a job frame arrives over `GET /api/sync/stream`,
// then the runner's record — `/api/dag`, refetched on each `dag_changed`
// frame — catches up. The job's id is the run id, which is what lets the
// page tell "the record is about this run" from "still about the last".
describe("the live sequence, while the fetched record is still stale", () => {
  // Last run's record: closed, and about a different run entirely.
  const stalePoll: DagRun = {
    run_id: "yesterday",
    started_at: T.yesterday,
    finished_at: T.yesterday,
    live: false,
  };
  const thisRun: DagRun = {
    run_id: "job-1",
    started_at: T.runStart,
    finished_at: null,
    live: true,
  };

  const frames: {
    note: string;
    state: SyncJob["state"];
    fetched: DagRun;
    step: DagStep | undefined;
  }[] = [
    {
      note: "enqueue handler publishes the moment the row is written",
      state: "pending",
      fetched: stalePoll,
      step: undefined,
    },
    {
      note: "worker claimed it; the record still describes yesterday",
      state: "running",
      fetched: stalePoll,
      step: dagStep({ current_state: "succeeded" }),
    },
    {
      note: "the runner's record for this run lands, the step dispatched",
      state: "running",
      fetched: thisRun,
      step: dagStep({
        current_state: "running",
        progress: {
          msg: "page 3",
          metrics: { done: 3, queued: 7 },
          errors: 0,
          updated_at: T.runStart,
        },
      }),
    },
    {
      note: "further into the same step",
      state: "running",
      fetched: thisRun,
      step: dagStep({
        current_state: "running",
        progress: {
          msg: "page 7",
          metrics: { done: 7, queued: 3 },
          errors: 0,
          updated_at: T.runStart,
        },
      }),
    },
  ];

  /// Compose one frame the way the view does, then read the row.
  function read(frame: (typeof frames)[number], id: string) {
    const active = frame.state === "pending" || frame.state === "running";
    const j = job({ state: frame.state, started_at: active ? T.jobStart : null });
    const run = effectiveRun(frame.fetched, frame.state === "running" ? j : undefined);
    return stepStatus({
      id,
      step: stepForRun(frame.step, !!run?.synthesized),
      run,
      claim: claimedBy(steps(), [j]).get(id),
    });
  }

  it("says Queued from the enqueue frame — before any runner exists", () => {
    expect(read(frames[0], "a/ingest").key).toBe("queued");
  });

  it("says Queued while the record is still about yesterday", () => {
    // The stale record says Succeeded. That is yesterday's answer, and
    // `stepForRun` drops it: a synthesized run means the runner has
    // written nothing about this run yet.
    expect(read(frames[1], "a/ingest").key).toBe("queued");
  });

  it("says Running once the record for this run lands", () => {
    expect(read(frames[2], "a/ingest").key).toBe("running");
  });

  it("recognises the record by the job's id, whatever the queue says", () => {
    // The record for this run is the truth even while the queue row
    // still says the job is running after the runner closed the run.
    const closed = { ...thisRun, finished_at: T.runEnd, live: false };
    const run = effectiveRun(closed, job({ state: "running" }));
    expect(run).toBe(closed);
    expect(run?.synthesized).toBeUndefined();
  });

  it("never goes backwards across the live sequence", () => {
    const rank: Record<string, number> = { queued: 0, running: 1, succeeded: 2 };
    const seq = frames.map((f) => read(f, "a/ingest").key);
    expect(seq).toEqual(["queued", "queued", "running", "running"]);
    for (let i = 1; i < seq.length; i++) {
      expect(rank[seq[i]]).toBeGreaterThanOrEqual(rank[seq[i - 1]]);
    }
  });

  it("does not invent a live run when nothing is running", () => {
    // `effectiveRun` may only override a stale record on evidence. With
    // no running job there is none, and the polled answer stands.
    expect(effectiveRun(stalePoll, undefined)).toBe(stalePoll);
    expect(effectiveRun(stalePoll, job({ state: "pending" }))).toBe(stalePoll);
  });

  it("keeps last_run while dropping a stale current_state", () => {
    const step = dagStep({
      current_state: "succeeded",
      last_run: {
        run_id: "yesterday",
        started_at: T.yesterday,
        finished_at: T.yesterday,
        status: "succeeded",
        attempts: 1,
        error: null,
      },
    });
    const fresh = stepForRun(step, true);
    expect(fresh?.current_state).toBeNull();
    expect(fresh?.last_run?.status).toBe("succeeded");
    expect(stepForRun(step, false)).toBe(step);
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
    expect(waitingOn(steps(), "unified_index/grid_index", finishedNone)).toEqual([
      "a/render_markdown",
      "b/render_markdown",
    ]);
    // Only the direct ones. `a/ingest` is upstream too, but naming the
    // whole transitive set is the rest of the pipeline.
    expect(waitingOn(steps(), "a/render_markdown", finishedNone)).toEqual(["a/ingest"]);
  });

  it("drops inputs that already finished this run", () => {
    const done = (id: string) => id === "a/render_markdown";
    expect(waitingOn(steps(), "unified_index/grid_index", done)).toEqual(["b/render_markdown"]);
    expect(waitingOn(steps(), "unified_index/grid_index", finishedAll)).toEqual([]);
  });

  it("a source step waits on nothing upstream", () => {
    expect(waitingOn(steps(), "a/ingest", finishedNone)).toEqual([]);
  });

  function queuedDetail(id: string, blockers: string[], state: SyncJob["state"]) {
    const j = job({ state, source_ids: "a/ingest", started_at: state === "pending" ? null : T.jobStart });
    return stepStatus({
      id,
      step: undefined,
      run: null,
      claim: claimedBy(steps(), [j]).get(id),
      waitingOn: blockers,
    }).detail;
  }

  it("says what it is behind when something upstream is outstanding", () => {
    expect(queuedDetail("a/render_markdown", ["a/ingest"], "running")).toBe(
      "Waiting for a/ingest to finish, in the sync of a/ingest.",
    );
  });

  it("reads as a sentence with more than one blocker", () => {
    expect(
      queuedDetail("unified_index/grid_index", ["a/render_markdown", "b/render_markdown"], "running"),
    ).toBe(
      "Waiting for a/render_markdown and b/render_markdown to finish, in the sync of a/ingest.",
    );
  });

  it("distinguishes a job not yet started from one already going", () => {
    // Nothing upstream to name, so the job itself is the answer — and
    // "hasn't started" and "started, not my turn yet" are different
    // things to be told.
    // On `a/ingest`'s own row "the sync of a/ingest" is a mouthful that says
    // nothing — it *is* that row. Named only when it is someone else's.
    expect(queuedDetail("a/ingest", [], "pending")).toBe("Waiting for this sync to start.");
    expect(queuedDetail("a/ingest", [], "running")).toBe("Waiting its turn in this sync.");
  });
});

describe("a second sync of a row that has already run", () => {
  // The frame this exists for is the one `manager2-sync.spec.ts` caught
  // intermittently as
  // `went backwards: ["Queued","Succeeded","Running","Succeeded"]`.
  const PREVIOUS: DagRun = {
    run_id: T.yesterday,
    started_at: T.yesterday,
    finished_at: T.yesterday,
    live: false,
  };
  const THIS: DagRun = {
    run_id: "job-1",
    started_at: T.runStart,
    finished_at: null,
    live: true,
  };
  const alreadySucceeded = dagStep({
    // The previous run's leftovers, both of them.
    current_state: "succeeded",
    last_run: {
      run_id: T.yesterday,
      started_at: T.yesterday,
      finished_at: T.yesterday,
      status: "succeeded",
      attempts: 1,
      error: null,
    },
  });

  /// The grid's own composition, which is what the view does per row:
  /// pick the run to judge against, then drop what the fetched record
  /// says about a run it has not caught up with.
  function paint(jobs: SyncJob[], fetched: DagRun, step: DagStep): StatusView {
    const live = jobs.find((j) => j.state === "running");
    const run = effectiveRun(fetched, live);
    return stepStatus({
      id: "a/ingest",
      step: stepForRun(step, !!run?.synthesized),
      run,
      claim: claimedBy(steps(), jobs).get("a/ingest"),
    });
  }

  it("is queued the moment the job is, not still showing the last run", () => {
    expect(
      paint([job({ state: "pending", started_at: null })], PREVIOUS, alreadySucceeded).key,
    ).toBe("queued");
  });

  it("stays queued once the worker starts it, before the record catches up", () => {
    // The failing frame. The job is running; `/api/dag` has not been
    // rewritten yet, so everything it says is about yesterday.
    expect(paint([job({ state: "running" })], PREVIOUS, alreadySucceeded).key).toBe("queued");
  });

  it("reaches Running when the record for this run lands", () => {
    const running = dagStep({ ...alreadySucceeded, current_state: "running" });
    expect(paint([job({ state: "running" })], THIS, running).key).toBe("running");
  });

  it("never goes backwards across the whole re-sync", () => {
    const running = dagStep({ ...alreadySucceeded, current_state: "running" });
    const seen = [
      paint([job({ state: "pending", started_at: null })], PREVIOUS, alreadySucceeded),
      paint([job({ state: "running" })], PREVIOUS, alreadySucceeded),
      paint([job({ state: "running" })], THIS, running),
      // The record lands, describing the run that just ended.
      stepStatus({
        id: "a/ingest",
        step: dagStep({
          current_state: "succeeded",
          last_run: {
            run_id: "job-1",
            started_at: T.runStart,
            finished_at: T.aDone,
            status: "succeeded",
            attempts: 1,
            error: null,
          },
        }),
        run: { ...THIS, finished_at: T.runEnd, live: false },
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
    expect(hold("a/ingest", "job-1", view("queued")).key).toBe("queued");
    expect(hold("a/ingest", "job-1", view("running")).key).toBe("running");
    // The frame the e2e caught: claim still running, no `current_state`
    // from either source, so `stepStatus` reads it as Queued.
    expect(hold("a/ingest", "job-1", view("queued")).key).toBe("running");
    expect(hold("a/ingest", "job-1", view("succeeded")).key).toBe("succeeded");
  });

  it("lets the next sync of the same row start at Queued again", () => {
    const hold = statusFloor();
    hold("a/ingest", "job-1", view("running"));
    hold("a/ingest", "job-1", view("succeeded"));
    // A new job is a new key, so the floor lifts. Without this a row
    // could never be seen queued twice.
    expect(hold("a/ingest", "job-2", view("queued")).key).toBe("queued");
  });

  it("keeps rows apart", () => {
    const hold = statusFloor();
    hold("a/ingest", "job-1", view("running"));
    expect(hold("b/ingest", "job-1", view("queued")).key).toBe("queued");
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
    hold("a/ingest", "job-1", running);
    expect(hold("a/ingest", "job-1", view("queued"))).toEqual(running);
  });

  it("passes through a status it cannot rank rather than freezing on it", () => {
    // A vocabulary this table has not met is exactly when holding a row
    // would be worst: we would be pinning it at a rank we invented.
    const hold = statusFloor();
    hold("a/ingest", "job-1", view("running"));
    expect(hold("a/ingest", "job-1", view("something_new")).key).toBe("something_new");
    // ...and having forgotten it, the next real status is free too.
    expect(hold("a/ingest", "job-1", view("queued")).key).toBe("queued");
  });

  /// Two statuses in the column are *not* points in a run, and must
  /// stay outside the ranking. Asserted absent rather than skipped:
  /// giving one a rank would let the floor pin a row at a status that
  /// describes a config which no longer exists.
  const CONFIG_STATUSES = ["config_rejected", "config_blocked"];

  it("ranks every run status, and deliberately ranks neither config status", () => {
    // Total over the run vocabulary on purpose: an unranked run status
    // silently opts out of the floor, which is the one gap that would
    // go unnoticed.
    for (const key of Object.keys(STATUS_LABEL)) {
      if (CONFIG_STATUSES.includes(key)) {
        expect(STATUS_RANK[key], `${key} must stay unranked`).toBeUndefined();
        continue;
      }
      expect(STATUS_RANK[key], `${key} has no rank`).toBeDefined();
    }
  });

  /// The floor exists to stop a status going backwards *within a run*.
  /// A config edit is the case where going backwards is the truth:
  /// the entry left the pipeline, so last run's `Succeeded` is no
  /// longer a fact about it. This is the whole reason the config
  /// statuses are unranked.
  it("lets a dropped entry override a status it already showed", () => {
    const hold = statusFloor();
    hold("a/ingest", "job-1", view("succeeded"));
    const dropped = view("config_rejected");
    expect(hold("a/ingest", "job-1", dropped).key).toBe("config_rejected");
    // …and the row's memory is cleared with it, so fixing the config
    // does not leave it pinned at the dropped state either.
    expect(hold("a/ingest", "job-1", view("queued")).key).toBe("queued");
  });
});

describe("an entry the config loader dropped", () => {
  const diag = (severity: "rejected" | "blocked", message: string, help?: string) => ({
    severity,
    entry: { kind: "step" as const, index: null, id: "slack/ingest" },
    message,
    help: help ?? null,
    span: null,
    line: 7,
    column: 1,
  });

  /// The failure #209 is about: a row that reads "Up to date" from a
  /// run taken under a config that no longer loads this step. The
  /// table looks healthy and the data has silently stopped moving.
  it("outranks whatever the runner's record still remembers", () => {
    const step: DagStep = {
      id: "slack/ingest",
      current_state: null,
      last_run: {
        run_id: "r1",
        started_at: "2026-09-01T10:00:00+00:00",
        finished_at: "2026-09-01T10:01:00+00:00",
        status: "succeeded",
        error: null,
        attempts: 1,
      },
      progress: null,
    } as unknown as DagStep;

    const healthy = stepStatus({ id: "slack/ingest", step, run: null, claim: undefined });
    expect(healthy.key).toBe("succeeded");

    const now = stepStatus({
      id: "slack/ingest",
      step,
      run: null,
      claim: undefined,
      dropped: diag("rejected", "unknown field `title`"),
    });
    expect(now.key).toBe("config_rejected");
    expect(now.detail).toContain("title");
    // No timestamp: it would describe a run this config never took
    // part in, and the "Last synced" column reads `at`.
    expect(now.at).toBeNull();
  });

  /// `blocked` and `rejected` drop the entry alike and say different
  /// things — the fix for one is at this entry, for the other it is
  /// somewhere else, and the row has to carry that distinction.
  it("distinguishes a broken entry from one broken by another", () => {
    const blocked = stepStatus({
      id: "slack/ingest",
      step: undefined,
      run: null,
      claim: undefined,
      dropped: diag("blocked", "input \"slack/x\" names no declared step", "fix that entry"),
    });
    expect(blocked.key).toBe("config_blocked");
    expect(blocked.detail).toContain("fix that entry");
  });

  /// A warning drops nothing, so a row carrying one still runs and
  /// still deserves the runner's own status. Manager2 filters these
  /// out before they get here; this pins that they would be harmless
  /// even if it didn't.
  it("is not triggered by a status the loader still ran", () => {
    const v = stepStatus({ id: "slack/ingest", step: undefined, run: null, claim: undefined });
    expect(v.key).toBe("never_run");
  });
});
