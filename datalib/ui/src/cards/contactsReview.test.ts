import { describe, expect, it } from "vitest";
import {
  clusterLabels,
  filterReview,
  membershipLabel,
  orderReview,
  reviewHandle,
  type ReviewFilter,
  type ReviewRow,
} from "./contactsReview";

function row(key: string, over: Partial<ReviewRow> = {}): ReviewRow {
  return {
    key,
    contact_id: null,
    who: { id: `mailto:${key}`, label: key },
    real: null,
    label_values: [],
    confidence: null,
    status: "waiting for you",
    awaiting: true,
    why: "",
    agent: "claude-test",
    ...over,
  };
}

const any: ReviewFilter = { waiting: false, real: "", label: "", text: "" };

describe("filterReview", () => {
  const rows = [
    row("troi@enterprise.org", { real: true, label_values: ["real", "crew"] }),
    row("offers@ferengi.test", { real: false, awaiting: false }),
    row("q@continuum.test", { why: "omnipotent nuisance" }),
  ];
  const keys = (f: Partial<ReviewFilter>) => filterReview(rows, { ...any, ...f }).map((r) => r.key);

  it("keeps what waits for a person", () => {
    expect(keys({ waiting: true })).toEqual(["troi@enterprise.org", "q@continuum.test"]);
  });
  it("narrows by what real says: in, out, or nobody has said", () => {
    expect(keys({ real: "in" })).toEqual(["troi@enterprise.org"]);
    expect(keys({ real: "out" })).toEqual(["offers@ferengi.test"]);
    expect(keys({ real: "unknown" })).toEqual(["q@continuum.test"]);
  });
  it("narrows by a cluster label and by the agent's reasons", () => {
    expect(keys({ label: "crew" })).toEqual(["troi@enterprise.org"]);
    expect(keys({ text: "NUISANCE" })).toEqual(["q@continuum.test"]);
  });
});

describe("clusterLabels", () => {
  it("counts every label but real, most used first", () => {
    const rows = [
      row("a", { label_values: ["real", "crew", "bridge"] }),
      row("b", { label_values: ["crew"] }),
    ];
    expect(clusterLabels(rows)).toEqual([
      { label: "crew", count: 2 },
      { label: "bridge", count: 1 },
    ]);
  });
});

describe("reviewHandle", () => {
  it("is the chip's handle, or null for a contact with none", () => {
    expect(reviewHandle(row("troi@enterprise.org"))).toBe("email:troi@enterprise.org");
    expect(
      reviewHandle(row("x", { who: { id: "datalib:contact/abc", label: "Troi" } })),
    ).toBeNull();
  });
});

describe("orderReview", () => {
  it("puts what waits for you first, least sure first, then the rest as they came", () => {
    const rows = [
      row("done", { awaiting: false, confidence: 0.1 }),
      row("sure", { confidence: 0.97 }),
      row("unsaid"),
      row("close", { confidence: 0.55 }),
    ];
    expect(orderReview(rows).map((r) => r.key)).toEqual(["close", "sure", "unsaid", "done"]);
  });
});

describe("membershipLabel", () => {
  it("reads the label back from a membership column's field", () => {
    expect(membershipLabel("label:spam")).toBe("spam");
    expect(membershipLabel("who")).toBeNull();
  });
});
