// A card's help text, by card id. One place rather than a field on
// each layout's slot, so the three layouts and the chrome's "?" button
// read and write the same thing. HTML, because a card is trusted code
// already (its source is `new Function`'d) and its help is prose it
// wrote itself.
import { computed, reactive, type ComputedRef } from "vue";

const helps = reactive(new Map<string, string>());

export function setCardHelp(cardId: string, html: string | null): void {
  if (html) helps.set(cardId, html);
  else helps.delete(cardId);
}

export function cardHelp(cardId: string): ComputedRef<string | null> {
  return computed(() => helps.get(cardId) ?? null);
}
