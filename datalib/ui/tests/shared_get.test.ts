// GETs asked in one task share a request (api.ts `sharedGet`).
import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchManageRows } from "@/api";

afterEach(() => vi.unstubAllGlobals());

function countedFetch(contentType = "application/json") {
  const fetch = vi.fn(
    async () =>
      new Response('{"rows":[]}', { status: 200, headers: { "Content-Type": contentType } }),
  );
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

describe("getJson", () => {
  /// A card's source may point a table at any path, and some paths
  /// serve what a stranger sent: a body is JSON only when it says so.
  it("refuses an answer that is not application/json, shared or not", async () => {
    for (const type of ["text/plain", "text/html", "application/octet-stream", ""]) {
      countedFetch(type);
      await expect(fetchManageRows(), type).rejects.toThrow(/not JSON/);
      await expect(fetchManageRows(false, new AbortController().signal), type).rejects.toThrow(
        /not JSON/,
      );
    }
  });

  it("takes application/json with a charset", async () => {
    countedFetch("application/json; charset=utf-8");
    await expect(fetchManageRows()).resolves.toEqual({ rows: [] });
    await expect(fetchManageRows(false, new AbortController().signal)).resolves.toEqual({
      rows: [],
    });
  });
});

describe("sharedGet", () => {
  /// The Dashboard's four sections each read the Sources table on every
  /// live frame: four identical requests where one answers them all.
  it("sends one request for the same GET asked in one task", async () => {
    const fetch = countedFetch();
    const answers = await Promise.all([fetchManageRows(), fetchManageRows(), fetchManageRows()]);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(answers[0]).toEqual({ rows: [] });
    // Each caller holds its own copy, free to change it.
    expect(answers[0]).not.toBe(answers[1]);
  });

  it("asks again in a later task, which may follow a change", async () => {
    const fetch = countedFetch();
    await fetchManageRows();
    await fetchManageRows();
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("keeps a request of its own for a caller that can abort", async () => {
    const fetch = countedFetch();
    const ac = new AbortController();
    await Promise.all([fetchManageRows(false, ac.signal), fetchManageRows()]);
    expect(fetch).toHaveBeenCalledTimes(2);
  });
});
