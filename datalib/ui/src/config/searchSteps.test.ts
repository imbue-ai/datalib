// A time-series source never gets an embed step, whichever caller asks
// for one; any other source still can.

import { describe, expect, it } from "vitest";

import { listSteps, setSearchSteps } from "./sourceSteps";

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
function = "qmd_aggregator"
inputs = []
`;

const searchStepIds = (text: string) =>
  listSteps(text)
    .filter((s) => s.function === "keyword_index" || s.function === "embed")
    .map((s) => s.id);

const aggregatorInputs = (text: string) =>
  listSteps(text).find((s) => s.function === "qmd_aggregator")?.inputs;

describe("setSearchSteps", () => {
  it("gives a garmin source a keyword index and no embed step", () => {
    const out = setSearchSteps(config("garmin"), "away-team", "keyword_and_embed");
    expect(searchStepIds(out)).toEqual(["away-team/keyword_index"]);
    expect(aggregatorInputs(out)).toEqual(["away-team/keyword_index"]);
  });

  it("still embeds a source of any other type", () => {
    const out = setSearchSteps(config("email"), "away-team", "keyword_and_embed");
    expect(searchStepIds(out)).toEqual(["away-team/keyword_index", "away-team/embed"]);
  });
});
