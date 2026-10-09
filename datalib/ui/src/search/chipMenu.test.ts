import { describe, expect, it } from "vitest";
import { chipMenu, NOBODY } from "@/cards/contacts";
import { entityMenu } from "@/cards/entities";
import { fieldChipMenu, toggleNegate } from "./chipMenu";
import { words } from "./queryText";

describe("a chip's menu in the search field", () => {
  it("leads with the field's own entries, then the chip's", () => {
    const slack = entityMenu("datalib:group/slack", "Work Slack");
    expect(fieldChipMenu("Work Slack", false, slack).map((e) => e.id)).toEqual([
      "edit-text",
      "toggle-negate",
      "copy-name",
      "copy-id",
      "copy-both",
      "open",
      "browse",
    ]);
    const [, negate, firstOfChip] = fieldChipMenu("Work Slack", true, slack);
    expect(negate.label).toBe("Include Work Slack instead");
    expect(firstOfChip.separator).toBe(true);
    const riker = chipMenu("email:riker@enterprise.org", "Will Riker", NOBODY, false);
    expect(fieldChipMenu("Will Riker", false, riker).map((e) => e.id)).toEqual([
      "edit-text",
      "toggle-negate",
      "copy-name",
      "copy-id",
      "copy-both",
      "search",
    ]);
  });

  it("excludes with a leading dash, and includes by taking it off", () => {
    const q = "is:document source_id:slack -step:slack/ingest";
    const [, plain, negated] = words(q);
    expect(toggleNegate(plain)).toEqual({ from: 12, to: 12, insert: "-" });
    expect(toggleNegate(negated)).toEqual({ from: 28, to: 29, insert: "" });
  });
});
