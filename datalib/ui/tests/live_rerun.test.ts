import { describe, expect, it } from "vitest";
import { oneAtATime } from "../src/live";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => (resolve = r));
  return { promise, resolve };
}

/** Every promise callback already queued has run. */
const settled = () => new Promise<void>((r) => setTimeout(r, 0));

describe("oneAtATime", () => {
  /// A search card re-ran its 4–10 s search on every index commit of a
  /// sync, every ~15 s for 20 minutes, aborting the run before it; the
  /// server finished each aborted search anyway, so they queued.
  it("runs once more after a run, however many frames came during it", async () => {
    const runs: ReturnType<typeof deferred>[] = [];
    const rerun = oneAtATime(() => {
      const d = deferred();
      runs.push(d);
      return d.promise;
    });

    rerun();
    rerun();
    rerun();
    rerun();
    expect(runs).toHaveLength(1);

    runs[0].resolve();
    await settled();
    expect(runs).toHaveLength(2);

    runs[1].resolve();
    await settled();
    expect(runs).toHaveLength(2);
  });

  it("starts at once when nothing is running, including after a failure", async () => {
    let n = 0;
    const rerun = oneAtATime(async () => {
      n += 1;
      throw new Error("the search failed");
    });
    rerun();
    expect(n).toBe(1);
    await settled();
    rerun();
    expect(n).toBe(2);
  });
});
