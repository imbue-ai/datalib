// What the chrome can ask of the card surface — reveal a card, open a
// search — without knowing which layout is showing. CardsView
// registers the active layout here; off the card surface (the gates)
// the toolbar navigates to a stack that holds the card instead.
import { ref } from "vue";
import router, { MANAGE_STACK } from "@/router";
import { encodeColumns } from "@/router/columns";
import { searchSource } from "@/cards/cardSources";

export const SOURCES_CARD = "sourcesView()";
export const LOG_CARD = "logView()";

export type SurfaceCommands = {
  // A new gallery card, revealed.
  addCard(): void;
  // Reveal the card whose source is `source`; open one if none is showing.
  showCard(source: string): void;
};

export const surface = ref<SurfaceCommands | null>(null);

export function showDataSources() {
  if (surface.value) surface.value.showCard(SOURCES_CARD);
  else void router.push(MANAGE_STACK);
}

/// The toolbar's search box: a search card on `q`, beside whatever is
/// showing.
export function searchFor(q: string) {
  const source = searchSource(q);
  if (surface.value) surface.value.showCard(source);
  else void router.push(encodeColumns([{ code: source, size: null, state: "" }]));
}
