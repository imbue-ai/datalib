// The wizard's form layout: what every catalog entry's `sections` must
// hold for the dialog to draw it, and how an answer is read from the
// values and written back. docs/dev/wizard_design.md.

import { describe, expect, it } from "vitest";
import { CATALOG, type CatalogEntry } from "./catalog";
import {
  answerIsEmpty,
  applyAnswer,
  chosenAnswer,
  layoutOf,
  seedFieldValues,
  type Row,
} from "./sourceSteps";

const WIZARDS = CATALOG.filter((e) => e.wizard);
const entry = (name: string): CatalogEntry => WIZARDS.find((e) => e.defaultName === name)!;
const rows = (e: CatalogEntry): Row[] => {
  const layout = layoutOf(e, true);
  return [...layout.basic, ...layout.advanced];
};
const row = (e: CatalogEntry, heading: string): Row => rows(e).find((r) => r.heading === heading)!;

describe("every wizard's sections", () => {
  /// A target a section misspells draws nothing, and the field it meant
  /// lands in Advanced options with no error anywhere.
  it.each(WIZARDS)("$label names only fields it has", (e) => {
    const targets = new Set((e.fields ?? []).map((f) => f.target));
    for (const section of e.sections ?? []) {
      const named = [
        ...(section.fields ?? []),
        ...(section.answers ?? []).flatMap((a) => [
          ...(a.fields ?? []),
          ...Object.keys(a.sets ?? {}),
        ]),
      ];
      for (const target of named)
        expect(targets, `${section.heading}: ${target}`).toContain(target);
    }
  });

  /// A tickbox is drawn with its own label beside it, so one that falls
  /// through to Advanced options alone would say its label twice. Every
  /// one is placed under a heading instead.
  it.each(WIZARDS)("$label places every tickbox in a section", (e) => {
    const solo = rows(e)
      .filter((r) => r.solo)
      .flatMap((r) => r.fields);
    expect(solo.filter((f) => f.kind === "bool").map((f) => f.target)).toEqual([]);
  });

  /// Adding a source with nothing touched has to be the sensible
  /// default, which the first answer is by convention.
  it.each(WIZARDS)("$label starts every question on its first answer", (e) => {
    const values = seedFieldValues(e);
    for (const r of rows(e)) {
      if (r.answers) expect(chosenAnswer(r.answers, values), r.heading).toBe(0);
    }
  });
});

describe("layoutOf", () => {
  it("puts a field no section names in Advanced options, under its own label", () => {
    const advanced = layoutOf(entry("slack"), true).advanced;
    expect(advanced.map((r) => r.heading)).toEqual(["Copy until", "Edit-catcher window (days)"]);
    expect(advanced.every((r) => r.solo)).toBe(true);
  });

  it("leaves the account of a source that signs in to the account row", () => {
    const targets = rows(entry("gmail")).flatMap((r) => r.fields.map((f) => f.target));
    expect(targets).not.toContain("latchkey_settings.account");
    // CalDAV has no sign-in of its own, so its account is a plain field.
    expect(rows(entry("caldav_calendar")).map((r) => r.heading)).toContain("Latchkey account");
  });

  it("drops the render fields of a source that is not rendered", () => {
    const headings = (renders: boolean) =>
      layoutOf(entry("gmail"), renders).advanced.map((r) => r.heading);
    expect(headings(true)).toContain("Render only these labels");
    expect(headings(false)).not.toContain("Render only these labels");
  });
});

describe("a question's answers", () => {
  const slack = entry("slack");
  const channels = row(slack, "Which channels?").answers!;
  const dms = row(slack, "Direct messages?").answers!;

  it("reads the answer back from the values a config holds", () => {
    const values = seedFieldValues(slack);
    expect(chosenAnswer(channels, { ...values, "api.all_channels": true })).toBe(1);
    expect(chosenAnswer(channels, { ...values, "api.channels": ["bridge"] })).toBe(2);
    // A channel list wins over the switch, as it does in the downloader.
    expect(
      chosenAnswer(channels, { ...values, "api.all_channels": true, "api.channels": ["bridge"] }),
    ).toBe(2);
    expect(chosenAnswer(dms, { ...values, "api.dms": true })).toBe(1);
    expect(
      chosenAnswer(dms, { ...values, "api.dms": true, "api.dm_conversations": ["D_RIKER"] }),
    ).toBe(2);
    // A DM list left behind with DMs off is not "only these".
    expect(
      chosenAnswer(dms, { ...values, "api.dms": false, "api.dm_conversations": ["D_RIKER"] }),
    ).toBe(0);
  });

  it("writes what the answer sets and empties what the others showed", () => {
    const picked = {
      ...seedFieldValues(slack),
      "api.dms": true,
      "api.dm_conversations": ["D_RIKER"],
    };
    const all = applyAnswer(dms, 1, picked);
    expect(all["api.dms"]).toBe(true);
    expect(all["api.dm_conversations"]).toEqual([]);
    const none = applyAnswer(dms, 0, picked);
    expect(none["api.dms"]).toBe(false);
    expect(none["api.dm_conversations"]).toEqual([]);
  });

  it("is not answered while the fields it shows are all empty", () => {
    const values = seedFieldValues(slack);
    const only = channels[2]!.fields;
    expect(answerIsEmpty(only, values)).toBe(true);
    expect(answerIsEmpty(only, { ...values, "api.channels": ["bridge"] })).toBe(false);
    expect(answerIsEmpty(channels[0]!.fields, values)).toBe(false);
  });
});
