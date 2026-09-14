// The Activity cell's chips, from a step's numbers.

import { describe, expect, it } from "vitest";
import { activityChips, activityText, formatAge, formatRate } from "./activity";
import type { DagStepProgress } from "@/api";

function progress(over: Partial<DagStepProgress> = {}): DagStepProgress {
  return {
    msg: null,
    metrics: {},
    errors: 0,
    rates: {},
    progress_age_secs: null,
    log_age_secs: null,
    updated_at: "2026-09-14T10:00:00.000000+00:00",
    ...over,
  };
}

describe("the activity chips", () => {
  it("sums every queued series into one leading chip, breakdown on hover", () => {
    const chips = activityChips(
      progress({
        metrics: { "queued{from=slack/ingest}": 120, "queued{from=email/ingest}": 0, queued: 5 },
      }),
    );
    expect(chips[0]).toMatchObject({ kind: "queued", text: "125 queued" });
    expect(chips[0].title).toContain("120 from slack/ingest");
    expect(chips[0].title).toContain("5 of its own");
    expect(chips).toHaveLength(1);
  });

  it("reads idle, not queued, at zero", () => {
    expect(activityChips(progress({ metrics: { queued: 0 } }))[0].kind).toBe("idle");
  });

  it("shows a series with its rate only when it is moving", () => {
    const chips = activityChips(
      progress({
        metrics: { rows_upserted: 700, api_requests: 9 },
        rates: { rows_upserted: 20, api_requests: 0 },
      }),
    );
    expect(chips.map((c) => c.text)).toEqual(["rows_upserted 700 · 20/s", "api_requests 9"]);
  });

  it("flags a running step whose numbers stopped moving, and says whether it still talks", () => {
    const busy = activityChips(progress({ progress_age_secs: 120, log_age_secs: 10 }));
    expect(busy[0]).toMatchObject({ kind: "stalled", text: "no progress 2m" });
    expect(busy[0].title).toContain("still logging");

    const silent = activityChips(progress({ progress_age_secs: 120, log_age_secs: 110 }));
    expect(silent[0].title).toContain("silent");

    // Under the threshold, and a finished step (no ages): nothing.
    expect(activityChips(progress({ progress_age_secs: 30, log_age_secs: 1 }))).toEqual([]);
    expect(activityChips(progress())).toEqual([]);
  });

  it("closes with the warn/error count, and is empty for a step that said nothing", () => {
    expect(activityChips(progress({ errors: 3 }))[0]).toMatchObject({
      kind: "errors",
      text: "3 ⚠",
    });
    expect(activityText(null)).toBe("");
    expect(activityText(progress({ metrics: { done: 4 }, errors: 1 }))).toBe("done 4  1 ⚠");
  });
});

describe("the short forms", () => {
  it("keeps a rate to three significant figures or so", () => {
    expect(formatRate(0.25)).toBe("0.3/s");
    expect(formatRate(20)).toBe("20/s");
    expect(formatRate(1234)).toBe("1.2k/s");
  });

  it("picks the unit a person would", () => {
    expect(formatAge(45)).toBe("45s");
    expect(formatAge(130)).toBe("2m");
    expect(formatAge(5400)).toBe("1.5h");
  });
});
