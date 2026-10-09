// What a chip in the search field does beyond what it does everywhere:
// its right-click menu is the field's entries followed by the chip's own
// (`entityMenu` for a source, group or step, `chipMenu` for a person), and
// the edits the field's entries make to the query.
import type { ChipMenuId } from "@/cards/contacts";
import type { EntityMenuId } from "@/cards/entities";
import type { Word } from "./queryText";

export type FieldMenuId = "edit-text" | "toggle-negate" | EntityMenuId | ChipMenuId;
export type FieldMenuEntry = { id: FieldMenuId; label: string; separator?: boolean };

/** The menu on a chip in the field: edit it as text, exclude or include
 *  what it names, then `own`, everything the chip offers anywhere. */
export function fieldChipMenu(
  name: string,
  negate: boolean,
  own: FieldMenuEntry[],
): FieldMenuEntry[] {
  const field: FieldMenuEntry[] = [
    { id: "edit-text", label: "Edit as text" },
    { id: "toggle-negate", label: negate ? `Include ${name} instead` : `Exclude ${name}` },
  ];
  const chip = own.map((e, i) => (i === 0 ? { ...e, separator: true } : e));
  return [...field, ...chip];
}

/** The change that excludes what a term matches, or includes it again:
 *  its leading `-`. */
export function toggleNegate(word: Word): { from: number; to: number; insert: string } {
  return word.negate
    ? { from: word.from, to: word.from + 1, insert: "" }
    : { from: word.from, to: word.from, insert: "-" };
}
