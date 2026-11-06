// Which search card the toolbar's box is the search box of. Search cards say
// here that they exist and when they show; the box reads and runs the query of
// the one last used among those on screen. A layout can show two side by side,
// so "the one showing" is not enough.
import { computed, shallowRef, type Ref } from "vue";

export type SearchCard = {
  id: string;
  /** The query, which the card owns. */
  query: Ref<string>;
  /** The query as last run. */
  settled: Ref<string>;
  run(q: string): void;
  openFeedback(ev: MouseEvent): void;
};

export type CardEntry = { card: SearchCard; shown: boolean; usedAt: number };

/// The card the box speaks for: the last used of those on screen, or none.
export function activeOf(entries: readonly CardEntry[]): SearchCard | null {
  let active: CardEntry | null = null;
  for (const e of entries) {
    if (e.shown && (!active || e.usedAt > active.usedAt)) active = e;
  }
  return active?.card ?? null;
}

/// What the box should hold once the card it speaks for, or that card's query,
/// changes: the card's query, unless the person is typing. A card comes on
/// screen a moment after the page does, and what was typed by then is theirs.
export function draftAfter(draft: string, typing: boolean, cardQuery: string | undefined): string {
  return typing && draft !== "" ? draft : (cardQuery ?? "");
}

const entries = shallowRef<CardEntry[]>([]);
let clock = 0;

export const activeSearch = computed(() => activeOf(entries.value));

function update(id: string, change: (e: CardEntry) => CardEntry) {
  entries.value = entries.value.map((e) => (e.card.id === id ? change(e) : e));
}

export function registerSearchCard(card: SearchCard) {
  entries.value = [...entries.value, { card, shown: false, usedAt: ++clock }];
  return {
    /// Coming on screen counts as being used: it is what was asked for.
    setShown: (shown: boolean) =>
      update(card.id, (e) => ({ ...e, shown, usedAt: shown ? ++clock : e.usedAt })),
    used: () => update(card.id, (e) => ({ ...e, usedAt: ++clock })),
    remove: () => {
      entries.value = entries.value.filter((e) => e.card.id !== card.id);
    },
  };
}
