// `personView(handle, { seenIn })` in card source: one person, as your
// contact and each source has them (see cards/PersonCard.ce.vue). A chip
// opens it through `personSource` in cardSources.ts.
import PersonCard from "../PersonCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function personView(handle: string, opts: { seenIn?: string | null } = {}): CardRender {
  return vueCard(PersonCard, { handle, seenIn: opts.seenIn ?? null });
}
