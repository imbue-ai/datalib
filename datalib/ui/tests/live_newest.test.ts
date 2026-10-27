import { describe, expect, it, vi } from "vitest";
import { newestAnswer } from "../src/live";

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

/** Every promise callback already queued has run. */
const settled = () => new Promise<void>((r) => setTimeout(r, 0));

describe("newestAnswer", () => {
  /// The launch's migrate pass sends an `upgrade_changed` per step state
  /// change, ~150 on the TNG root in about a second, and the page asked
  /// for `/api/config` on every one of them.
  it("fetches at most twice for a burst of asks", async () => {
    const fetches: ReturnType<typeof deferred<number>>[] = [];
    const kept: number[] = [];
    const ask = newestAnswer(
      () => {
        const d = deferred<number>();
        fetches.push(d);
        return d.promise;
      },
      (n) => kept.push(n),
    );

    for (let i = 0; i < 150; i++) ask();
    expect(fetches).toHaveLength(1);

    fetches[0].resolve(1);
    await settled();
    expect(fetches).toHaveLength(2);
    expect(kept).toEqual([]);

    fetches[1].resolve(2);
    await settled();
    expect(fetches).toHaveLength(2);
    expect(kept).toEqual([2]);
  });

  /// The first-run screen drops its gate on a guess, then asks; an answer
  /// already in flight from before the guess must not put the gate back.
  it("drops an answer that an ask made while it was in flight outdates", async () => {
    const fetches: ReturnType<typeof deferred<string>>[] = [];
    const kept: string[] = [];
    const ask = newestAnswer(
      () => {
        const d = deferred<string>();
        fetches.push(d);
        return d.promise;
      },
      (s) => kept.push(s),
    );

    ask();
    ask();
    fetches[0].resolve("before");
    await settled();
    expect(kept).toEqual([]);
    fetches[1].resolve("after");
    await settled();
    expect(kept).toEqual(["after"]);

    ask();
    fetches[2].resolve("alone");
    await settled();
    expect(kept).toEqual(["after", "alone"]);
  });
});

describe("newestAnswer with spacing", () => {
  /// A fetch that answers in a millisecond kept pace with the migrate
  /// pass's frames, so the page still asked for `/api/config` about 80
  /// times on one launch of the TNG root.
  it("asks at most once per spacing however fast the frames come", async () => {
    vi.useFakeTimers();
    try {
      let fetches = 0;
      const ask = newestAnswer(
        async () => {
          fetches += 1;
          await new Promise((r) => setTimeout(r, 1));
        },
        () => {},
        { spacing: 250 },
      );
      for (let t = 0; t < 1000; t += 5) {
        ask();
        await vi.advanceTimersByTimeAsync(5);
      }
      await vi.advanceTimersByTimeAsync(1000);
      expect(fetches).toBeGreaterThanOrEqual(4);
      expect(fetches).toBeLessThanOrEqual(5);
    } finally {
      vi.useRealTimers();
    }
  });
});
