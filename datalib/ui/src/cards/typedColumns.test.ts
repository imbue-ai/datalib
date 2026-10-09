import { describe, expect, it } from "vitest";
import { copyText, typedColumns } from "./typedColumns";
import type { ColumnSpec, Identity } from "@/api";
import type { Formatter } from "@slickgrid-universal/common";

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

/// An identity whose id names a person draws as a chip, from what the
/// grid has resolved; any other identity draws as icon and label.
describe("an identity cell that names a person", () => {
  const spec: ColumnSpec = {
    field: "author_ref",
    header: "Author",
    type: "identity",
    default_visible: true,
    editable: false,
  };
  const riker: Identity = { id: "mailto:riker@enterprise.org", label: "Will Riker", icon: "email" };
  const draw = (chips: Parameters<typeof typedColumns>[1]["chips"], v: Identity) => {
    const [col] = typedColumns<Record<string, unknown>>([spec], { chips });
    const out = (col.formatter as Formatter)(0, 0, v.label, col, { author_ref: v }, {} as never);
    return out as HTMLElement;
  };

  it("is a chip link with the handle, resolved to the contact when the grid knows one", () => {
    const unresolved = draw({ who: () => undefined, canLink: () => false }, riker);
    expect(unresolved.matches("a.chip.handle-chip.handle-unresolved[data-handle]")).toBe(true);
    expect(unresolved.dataset.handle).toBe("email:riker@enterprise.org");
    expect(unresolved.getAttribute("href")).toBe("mailto:riker@enterprise.org");
    expect(unresolved.textContent).toBe("Will Riker");
    const mine = {
      source_id: "datalib_contacts",
      key: "c1",
      kind: "person" as const,
      names: ["William T. Riker"],
      handles: [],
      org: null,
      title: null,
      seen: null,
    };
    const resolved = draw(
      { who: () => ({ mine, sourceContacts: [] }), canLink: () => true },
      riker,
    );
    expect(resolved.classList.contains("handle-resolved")).toBe(true);
    expect(resolved.textContent).toBe("WWilliam T. Riker");
    expect(resolved.querySelector(".handle-initial")?.textContent).toBe("W");
  });

  it("draws an identity that is not a person as icon and label", () => {
    const group: Identity = { id: "slack", label: "Slack", icon: "slack" };
    const cell = draw({ who: () => undefined, canLink: () => false }, group);
    expect(cell.classList.contains("tg-identity")).toBe(true);
    expect(cell.querySelector("a.chip")).toBeNull();
  });
});
