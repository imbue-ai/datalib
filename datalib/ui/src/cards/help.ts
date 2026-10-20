// A card's help text, by card id. One place rather than a field on
// each layout's slot, so every layout and the chrome's "?" button read
// and write the same thing. HTML, because only a card's own code sets
// it: a built-in view or an installed component, never its source's
// arguments.
import { computed, reactive, type ComputedRef } from "vue";

const helps = reactive(new Map<string, string>());

export function setCardHelp(cardId: string, html: string | null): void {
  if (html) helps.set(cardId, html);
  else helps.delete(cardId);
}

export function cardHelp(cardId: string): ComputedRef<string | null> {
  return computed(() => helps.get(cardId) ?? null);
}

/// Text going into help HTML. A value from a card's arguments came from
/// whoever wrote the link, so it goes in as text, never as markup.
export function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
