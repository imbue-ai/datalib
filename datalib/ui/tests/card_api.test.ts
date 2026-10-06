// The card scope (cards/cardScope.ts) ends at the first await, so a
// request says which card made it only when its api.ts function calls
// fetch before awaiting anything.
import { afterEach, describe, expect, it, vi } from "vitest";
import { cardApi } from "@/cards/cardApi";
import { cardBeingServed, type CardTag } from "@/cards/cardScope";
import type { CardCtx } from "@/cards/types";

const ctx = { cardId: "0192f6a0-0000-7000-8000-000000000000", cardType: "gridView" } as CardCtx;

/// Exports that are functions but make no request.
const NO_REQUEST = new Set(["remoteMediaUrl", "healthSnapshot"]);

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("cardApi", () => {
  /// Guards against an api.ts function that awaits before its fetch:
  /// its requests would quietly stop naming their card.
  it("runs every request an api function makes in its card's scope", async () => {
    const untagged: string[] = [];
    const silent: string[] = [];
    for (const [name, fn] of Object.entries(cardApi(ctx))) {
      if (typeof fn !== "function" || NO_REQUEST.has(name)) continue;
      const seen: (CardTag | null)[] = [];
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => {
          seen.push(cardBeingServed());
          return new Response("{}", { status: 200 });
        }),
      );
      try {
        await (fn as (...a: unknown[]) => unknown)("x", "x", "x");
      } catch {
        // Dummy arguments; only the request matters.
      }
      if (seen.length === 0) silent.push(name);
      else if (seen[0]?.id !== ctx.cardId) untagged.push(name);
    }
    expect(untagged).toEqual([]);
    expect(silent).toEqual([]);
  });

  it("leaves the scope when the call returns", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("{}")),
    );
    await cardApi(ctx).fetchDag();
    expect(cardBeingServed()).toBeNull();
  });
});
