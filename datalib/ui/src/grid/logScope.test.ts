import { describe, expect, it } from "vitest";
import { EVERYTHING, scopeOf, withScope } from "./logScope";

describe("a log panel's scope as search terms", () => {
  it("reads a run, a process and a step's attempt off the query", () => {
    expect(scopeOf("min_level:info run:r-1 step:slack/ingest attempt:2")).toEqual({
      run: "r-1",
      processId: null,
      step: "slack/ingest",
      attempt: 2,
    });
    expect(scopeOf("process_id:p-9")).toEqual({ ...EVERYTHING, processId: "p-9" });
    expect(scopeOf("level:warn")).toEqual(EVERYTHING);
  });

  /// A picker replaces the scope and leaves what was typed alone.
  it("writes a scope over the old one and keeps the rest of the query", () => {
    const q = "min_level:info run:r-1 step:slack/ingest attempt:2 thread:main";
    expect(withScope(q, { ...EVERYTHING, processId: "p-9" })).toBe(
      "min_level:info thread:main process_id:p-9",
    );
    expect(withScope(q, EVERYTHING)).toBe("min_level:info thread:main");
    expect(withScope("", { run: "r-2", processId: null, step: "a", attempt: 1 })).toBe(
      "run:r-2 step:a attempt:1",
    );
  });

  it("round-trips", () => {
    const scope = { run: "r-1", processId: "p-3", step: null, attempt: null };
    expect(scopeOf(withScope("msg:x", scope))).toEqual(scope);
  });
});
