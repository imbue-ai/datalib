import { describe, expect, it } from "vitest";
import { copyText } from "./typedColumns";

describe("what a cell copies as", () => {
  /// A pasted bug report has to say when, not "7 days ago".
  it("copies a stamp as the stamp, not as drawn", () => {
    const at = "2026-09-30T12:00:00+00:00";
    expect(copyText("timestamp", at)).toBe(at);
    expect(copyText("datetime", at)).toBe(at);
  });

  it("copies a resolved value as its label", () => {
    expect(copyText("identity", { id: "g1", label: "Slack", icon: "slack" })).toBe("Slack");
    expect(copyText("markdown_uuid", { id: "u1", label: "Standup notes" })).toBe("Standup notes");
    expect(copyText("markdown_uuid", "u1")).toBe("u1");
  });

  it("copies chips and a series as the words they show", () => {
    const chips = [
      { kind: "info", text: "a", title: "" },
      { kind: "info", text: "b", title: "" },
    ] as const;
    expect(copyText("chips", chips)).toBe("a, b");
    expect(copyText("timeseries", { value: 3, unit: "items", samples: [], window_secs: 60 })).toBe(
      "3 items",
    );
  });

  it("copies nothing for an empty cell or a row of buttons", () => {
    expect(copyText("text", null)).toBe("");
    expect(copyText("actions", [{ id: "sync", label: "Sync", enabled: true }])).toBe("");
  });
});
