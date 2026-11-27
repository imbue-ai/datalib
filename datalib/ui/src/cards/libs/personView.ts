// `personView(subject, { seenIn })` in card source: one person, as your
// contact and each source has them (see cards/PersonCard.ce.vue), by a
// handle, `contact:<id>` or `page:<id>`. A chip opens it through
// `personSource` in cardSources.ts.
import PersonCard from "../PersonCard.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import ContactEditor from "../ContactEditor.ce.vue";
import EntityChip from "../EntityChip.ce.vue";
import SuggestionItem from "../SuggestionItem.ce.vue";
import chipCss from "../chip.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function personView(subject: string, opts: { seenIn?: string | null } = {}): CardRender {
  return vueCard(
    PersonCard,
    { subject, seenIn: opts.seenIn ?? null },
    { styleSources: [ChipMenu, ContactEditor, EntityChip, SuggestionItem, chipCss] },
  );
}
