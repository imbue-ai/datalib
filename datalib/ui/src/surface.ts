// The two things the toolbar can ask of the card surface — reveal the
// sources card, start a new card — without knowing which layout is
// showing. CardsView registers the active layout here; off the card
// surface (the hidden /sources screen, the gates) the toolbar
// navigates to a stack that holds the card instead.
import { ref } from "vue";
import router, { MANAGE_STACK, NEW_CARD_STACK } from "@/router";

export const SOURCES_CARD = "sourcesView()";

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

export function newCard() {
  if (surface.value) surface.value.addCard();
  else void router.push(NEW_CARD_STACK);
}
