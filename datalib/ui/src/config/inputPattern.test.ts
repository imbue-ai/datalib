import { describe, expect, it } from "vitest";
import { expandInputs, namesStep, patternFunction } from "./inputPattern";

describe("input patterns", () => {
  it("are a star and one function", () => {
    expect(patternFunction("*/render_markdown")).toBe("render_markdown");
    for (const not of ["slack/render_markdown", "*", "*/", "*/a/b", "**/x", "*/x*", "a*/x"]) {
      expect(patternFunction(not)).toBeNull();
    }
  });

  it("name every group's step of that function and nothing deeper", () => {
    expect(namesStep("*/render_markdown", "slack/render_markdown")).toBe(true);
    expect(namesStep("*/render_markdown", "slack/ingest")).toBe(false);
    expect(namesStep("*/render_markdown", "render_markdown")).toBe(false);
    expect(namesStep("*/render_markdown", "a/b/render_markdown")).toBe(false);
    expect(namesStep("slack/ingest", "slack/ingest")).toBe(true);
  });

  it("expand in the file's order, without the step itself or a repeat", () => {
    const ids = [
      "slack/ingest",
      "slack/render_markdown",
      "mail/render_markdown",
      "index/render_markdown",
    ];
    expect(
      expandInputs(
        "index/render_markdown",
        ["slack/ingest", "*/render_markdown", "mail/render_markdown"],
        ids,
      ),
    ).toEqual(["slack/ingest", "slack/render_markdown", "mail/render_markdown"]);
  });
});
