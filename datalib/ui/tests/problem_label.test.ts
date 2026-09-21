import { describe, expect, it } from "vitest";
import { problemLabel } from "../src/cards/problems";

describe("problemLabel", () => {
  it("names the field and says what happened to it", () => {
    expect(problemLabel({ field: "created_at", reason: "coercion_failed", rule: null })).toBe(
      "`created_at` was not in the form expected and was left empty",
    );
    expect(problemLabel({ field: "uuid", reason: "no_identity", rule: null })).toBe(
      "`uuid` has no identity, so the record was dropped",
    );
  });
  it("speaks of the record when there is no field", () => {
    expect(problemLabel({ field: null, reason: "undeserializable", rule: null })).toBe(
      "this record could not be read",
    );
  });
  it("names the lossy rule that fired", () => {
    expect(
      problemLabel({ field: "text", reason: "deliberate_loss", rule: "pdf.strip_chrome" }),
    ).toBe("`text` was trimmed by the rule pdf.strip_chrome");
  });
  it("has a sentence for every reason the fetch and render stages report", () => {
    expect(problemLabel({ field: null, reason: "fetch_failed", rule: null })).toBe(
      "this record could not be fetched from the source",
    );
    expect(problemLabel({ field: null, reason: "render_failed", rule: null })).toBe(
      "this record could not be rendered",
    );
    expect(problemLabel({ field: "label", reason: "not_found", rule: null })).toBe(
      "`label` is not there upstream",
    );
    expect(problemLabel({ field: "channel", reason: "forbidden", rule: null })).toBe(
      "`channel` cannot be read with this credential",
    );
  });
  it("shows a word it does not know rather than hiding it", () => {
    // A store written by a newer build can carry a reason this one lacks.
    expect(problemLabel({ field: null, reason: "brand_new" as never, rule: null })).toBe(
      "this record: brand_new",
    );
  });
});
