// The per-step log reader behind double-clicking a Status cell.
//
// Every fixture below is a verbatim line from a real job log
// (`<root>/system/job-logs/<job>.log`), taken from the run that failed
// with the wizard's bad render-step id. Synthesizing them would have
// missed the two things that actually make this function necessary:
// a step's `log` events carry a whole `tracing` envelope escaped inside
// `msg`, and the steps emit the envelope *and* a bare copy of the same
// sentence back to back.

import { describe, expect, it } from "vitest";
import { stepLogLines } from "./stepLog";

const LOG = [
  `{"ts":"2026-09-02T09:44:06.932514+02:00","event":"run_plan","steps":["signal-work/raw","signal-work"]}`,
  `{"ts":"2026-09-02T09:44:06.933821+02:00","event":"step_start","step":"signal-work/raw","attempt":1}`,
  `{"ts":"2026-09-02T09:44:06.957300+02:00","event":"log","step":"signal-work/raw","level":"info","msg":"{\\"timestamp\\":\\"2026-09-02T07:44:06.957261Z\\",\\"level\\":\\"INFO\\",\\"fields\\":{\\"event\\":\\"signal_snapshot_already_ingested\\",\\"snapshot\\":\\"signal-backup-2026-06-08\\"},\\"target\\":\\"datalib_etl_signal::download\\",\\"filename\\":\\"datalib/backend/etl/providers/signal/src/download/mod.rs\\",\\"line_number\\":158,\\"threadId\\":\\"ThreadId(1)\\"}"}`,
  `{"ts":"2026-09-02T09:44:06.961822+02:00","event":"step_finish","step":"signal-work/raw","status":"succeeded"}`,
  `{"ts":"2026-09-02T09:44:06.962368+02:00","event":"step_start","step":"signal-work","attempt":1}`,
  `{"ts":"2026-09-02T09:44:06.973636+02:00","event":"progress_length","step":"signal-work","total":1}`,
  `{"ts":"2026-09-02T09:44:06.973699+02:00","event":"progress_inc","step":"signal-work","delta":1}`,
  `{"ts":"2026-09-02T09:44:06.974575+02:00","event":"step_finish","step":"signal-work","status":"failed","error":"step \\"signal-work\\" reported on \\"signal-work/rendered_md\\", but a step writes only the tree its id names (\\"signal-work\\")"}`,
  `signal-work/raw                  Succeeded { changed: 0 }`,
  `signal-work                      Failed { kind: Data }`,
].join("\n");

describe("narrowing a job log to one step", () => {
  it("keeps only the step asked for", () => {
    expect(stepLogLines(LOG, "signal-work/raw").map((l) => l.text)).toEqual([
      "started",
      "signal_snapshot_already_ingested snapshot=signal-backup-2026-06-08",
      "succeeded",
    ]);
  });

  it("carries the failure message, at error level", () => {
    const lines = stepLogLines(LOG, "signal-work");
    const last = lines[lines.length - 1];
    expect(last.level).toBe("error");
    expect(last.text).toContain("a step writes only the tree its id names");
    expect(last.text.startsWith("failed: ")).toBe(true);
  });

  it("drops the plain-text summary table, which names every step", () => {
    // "signal-work" appears in both trailing lines. Neither is JSON, and
    // keeping them would put the *other* step's outcome in this log.
    for (const l of stepLogLines(LOG, "signal-work")) {
      expect(l.text).not.toContain("Succeeded { changed");
    }
  });

  it("drops per-item progress but keeps the size of the work", () => {
    const texts = stepLogLines(LOG, "signal-work").map((l) => l.text);
    expect(texts).toContain("1 to do");
    expect(texts.some((t) => t.includes("delta"))).toBe(false);
  });

  it("unwraps a tracing envelope to its sentence and its fields", () => {
    const line = `{"ts":"2026-09-02T09:44:06.942054+02:00","event":"log","step":"s/raw","level":"info","msg":"{\\"timestamp\\":\\"2026-09-02T07:44:06.941924Z\\",\\"level\\":\\"INFO\\",\\"fields\\":{\\"message\\":\\"doltlite_raw::open: opening sqlite pool\\",\\"path\\":\\"/tmp/entities.doltlite_db\\"},\\"target\\":\\"datalib_etl::doltlite_raw\\",\\"filename\\":\\"x.rs\\",\\"line_number\\":500,\\"threadId\\":\\"ThreadId(1)\\"}"}`;
    expect(stepLogLines(line, "s/raw")).toEqual([
      {
        ts: "2026-09-02T09:44:06.942054+02:00",
        level: "info",
        text: "doltlite_raw::open: opening sqlite pool path=/tmp/entities.doltlite_db",
      },
    ]);
  });

  it("takes the envelope's level over the event's", () => {
    // The step reports `level:"error"` on the outer event too, but a
    // step that only set the envelope's must still come out red.
    const line = `{"ts":"t","event":"log","step":"s/raw","level":"info","msg":"{\\"level\\":\\"ERROR\\",\\"fields\\":{\\"message\\":\\"error: processor signal/download\\"}}"}`;
    expect(stepLogLines(line, "s/raw")[0].level).toBe("error");
  });

  it("collapses the envelope and the bare copy the steps emit together", () => {
    const dup = [
      `{"ts":"a","event":"log","step":"s/raw","level":"error","msg":"{\\"level\\":\\"ERROR\\",\\"fields\\":{\\"message\\":\\"caused by: $SIGNAL_BACKUP_PASSPHRASE not set\\"}}"}`,
      `{"ts":"b","event":"log","step":"s/raw","level":"info","msg":"caused by: $SIGNAL_BACKUP_PASSPHRASE not set"}`,
    ].join("\n");
    expect(stepLogLines(dup, "s/raw")).toHaveLength(1);
  });

  it("passes a plain, non-JSON msg through untouched", () => {
    const line = `{"ts":"t","event":"log","step":"s/raw","level":"warn","msg":"npx: installing @tobilu/qmd"}`;
    expect(stepLogLines(line, "s/raw")[0]).toEqual({
      ts: "t",
      level: "warn",
      text: "npx: installing @tobilu/qmd",
    });
  });

  it("says nothing for a step that never appears", () => {
    expect(stepLogLines(LOG, "unified_index/grid")).toEqual([]);
  });
});
