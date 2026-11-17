// How far into search the wizard puts a source: out of the grid step's
// `not_searched` for its words, an `embed` step for its meaning. A
// time-series source never gets an embed step, whichever caller asks for
// one; any other source still can.

import { describe, expect, it } from "vitest";

import { listSteps, qmdIndexingOf, setQmdSteps } from "./sourceSteps";

const config = (type: string) => `[[groups]]
id = "away-team"
type = "${type}"

[[steps]]
group = "away-team"
function = "ingest"

[[steps]]
group = "away-team"
function = "render_markdown"
inputs = ["away-team/ingest"]

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = ["*/render_markdown"]

[[steps]]
group = "unified_grid_and_search_index"
function = "embedding_map"
inputs = ["*/embed"]
`;

const embeds = (text: string) =>
  listSteps(text)
    .filter((s) => s.function === "embed")
    .map((s) => s.id);

const notSearched = (text: string) =>
  listSteps(text).find((s) => s.function === "grid_index")?.params?.not_searched;

describe("setQmdSteps", () => {
  it("gives a garmin source its words and no embed step", () => {
    const out = setQmdSteps(config("garmin"), "away-team", "keyword_and_embed");
    expect(embeds(out)).toEqual([]);
    expect(notSearched(out)).toBeUndefined();
    expect(qmdIndexingOf(listSteps(out), "away-team")).toBe("keyword");
  });

  it("still embeds a source of any other type, reading the grid step", () => {
    const out = setQmdSteps(config("email"), "away-team", "keyword_and_embed");
    expect(embeds(out)).toEqual(["away-team/embed"]);
    expect(listSteps(out).find((s) => s.id === "away-team/embed")?.inputs).toEqual([
      "unified_grid_and_search_index/grid_index",
    ]);
    expect(qmdIndexingOf(listSteps(out), "away-team")).toBe("keyword_and_embed");
  });

  /// Out of search is said on the grid step, and in again takes it back
  /// off the list, leaving no empty one behind.
  it("lists a source kept out of search in not_searched, and takes it off again", () => {
    const embedded = setQmdSteps(config("email"), "away-team", "keyword_and_embed");
    const out = setQmdSteps(embedded, "away-team", "none");
    expect(notSearched(out)).toEqual(["away-team"]);
    expect(embeds(out)).toEqual([]);
    expect(qmdIndexingOf(listSteps(out), "away-team")).toBe("none");
    const back = setQmdSteps(out, "away-team", "keyword");
    expect(notSearched(back)).toBeUndefined();
    expect(back).not.toContain("not_searched");
    expect(qmdIndexingOf(listSteps(back), "away-team")).toBe("keyword");
  });
});
