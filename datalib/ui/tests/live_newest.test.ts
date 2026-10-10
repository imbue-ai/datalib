import { describe, expect, it } from "vitest";
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
