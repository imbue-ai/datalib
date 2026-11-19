// How far into search the wizard puts a source: out of the `unified_grid_and_search_index` step's
// `not_searched` for its words, an `embed` step for its meaning. A
// time-series source never gets an embed step, whichever caller asks for
// one; any other source still can.

import { describe, expect, it } from "vitest";

import { listSteps, searchIndexingOf, setSearchSteps } from "./sourceSteps";

const config = (type: string) => `[[groups]]
id = "away-team"
type = "${type}"

[[steps]]
id = "away-team/ingest"

[[steps]]
id = "away-team/render_markdown"
inputs = ["away-team/ingest"]

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
inputs = ["*/render_markdown"]

[[steps]]
id = "unified_grid_and_search_index/embedding_map"
inputs = ["*/embed"]
`;

const embeds = (text: string) =>
  listSteps(text)
    .filter((s) => s.function === "embed")
    .map((s) => s.id);

const notSearched = (text: string) =>
  listSteps(text).find((s) => s.function === "unified_grid_and_search_index")?.params?.not_searched;

describe("setSearchSteps", () => {
  it("gives a garmin source its words and no embed step", () => {
    const out = setSearchSteps(config("garmin"), "away-team", "keyword_and_embed");
    expect(embeds(out)).toEqual([]);
    expect(notSearched(out)).toBeUndefined();
    expect(searchIndexingOf(listSteps(out), "away-team")).toBe("keyword");
  });

  it("still embeds a source of any other type, reading the `unified_grid_and_search_index` step", () => {
    const out = setSearchSteps(config("email"), "away-team", "keyword_and_embed");
    expect(embeds(out)).toEqual(["away-team/embed"]);
    expect(listSteps(out).find((s) => s.id === "away-team/embed")?.inputs).toEqual([
      "unified_grid_and_search_index/unified_grid_and_search_index",
    ]);
    expect(searchIndexingOf(listSteps(out), "away-team")).toBe("keyword_and_embed");
  });

  /// Out of search is said on the `unified_grid_and_search_index` step, and in again takes it back
  /// off the list, leaving no empty one behind.
  it("lists a source kept out of search in not_searched, and takes it off again", () => {
    const embedded = setSearchSteps(config("email"), "away-team", "keyword_and_embed");
    const out = setSearchSteps(embedded, "away-team", "none");
    expect(notSearched(out)).toEqual(["away-team"]);
    expect(embeds(out)).toEqual([]);
    expect(searchIndexingOf(listSteps(out), "away-team")).toBe("none");
    const back = setSearchSteps(out, "away-team", "keyword");
    expect(notSearched(back)).toBeUndefined();
    expect(back).not.toContain("not_searched");
    expect(searchIndexingOf(listSteps(back), "away-team")).toBe("keyword");
  });
});
