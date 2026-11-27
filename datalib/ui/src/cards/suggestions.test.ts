import { describe, expect, it } from "vitest";
import { runsOf, said, suggestionsAbout, type Suggestion } from "./suggestions";

function suggestion(extra: Partial<Suggestion>): Suggestion {
  return {
    subject: "contact:c1",
    contact_id: "c1",
    name: "Will Riker",
    new_contact: true,
    handle: null,
    handles: [],
    held: [],
    labels: [],
    agents: ["claude"],
    runs: ["run-1"],
    reasons: [],
    confidence: null,
    ...extra,
  };
}

const RIKER = "email:riker@enterprise.org";
const SLACK = "slack:T1/U1RIKER";

describe("said", () => {
  const sentence = (s: Suggestion) => {
    const w = said(s);
    return `${w.before} ${w.name ? `*${w.name}*` : ""}${w.after}`.trim();
  };

  it("says a new person, handles for one of yours, and a label in a sentence", () => {
    expect(sentence(suggestion({ handles: [RIKER, SLACK] }))).toBe("These two are *Will Riker*");
    expect(sentence(suggestion({ new_contact: false, handles: [SLACK] }))).toBe(
      "This is also *Will Riker*",
    );
    expect(
      sentence(
        suggestion({
          subject: "contact:q",
          name: "Q",
          labels: [{ label: "real", applies: false }],
        }),
      ),
    ).toBe("*Q* is not real (spam)");
    expect(
      sentence(
        suggestion({
          subject: RIKER,
          handle: RIKER,
          labels: [{ label: "real", applies: true }],
        }),
      ),
    ).toBe("This is real");
  });

  it("says why and who, each once", () => {
    const w = said(suggestion({ handles: [RIKER], reasons: ["signs as Will."] }));
    expect([w.because, w.by]).toEqual(["signs as Will", "claude (run-1)"]);
    expect(said(suggestion({ agents: [], runs: [] })).by).toBe("an agent");
  });
});

describe("suggestionsAbout", () => {
  it("finds what names one of the page's handles, or is about its contact", () => {
    const will = suggestion({ handles: [RIKER, SLACK] });
    const troi = suggestion({ subject: "contact:c2", contact_id: "c2", held: ["tel:+1"] });
    const spam = suggestion({
      subject: "email:q@q.test",
      contact_id: null,
      handle: "email:q@q.test",
    });
    const all = [will, troi, spam];
    expect(suggestionsAbout(all, [SLACK], null)).toEqual([will]);
    expect(suggestionsAbout(all, ["tel:+1"], null)).toEqual([troi]);
    expect(suggestionsAbout(all, [], "c2")).toEqual([troi]);
    expect(suggestionsAbout(all, ["email:q@q.test"], null)).toEqual([spam]);
  });
});

describe("runsOf", () => {
  it("gathers each run's subjects, for Yes to all", () => {
    const a = suggestion({ subject: "contact:a" });
    const b = suggestion({ subject: "contact:b", runs: ["run-1", "run-2"] });
    const c = suggestion({ subject: "contact:c", agents: ["codex"], runs: [] });
    expect(runsOf([a, b, c])).toEqual([
      { agent: "claude", run: "run-1", subjects: ["contact:a", "contact:b"] },
      { agent: "claude", run: "run-2", subjects: ["contact:b"] },
      { agent: "codex", run: null, subjects: ["contact:c"] },
    ]);
  });
});
