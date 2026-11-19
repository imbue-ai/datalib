// What the chrome can ask of the card surface — reveal a card, open a
// search — without knowing which layout is showing. CardsView registers
// the active layout here. The toolbar is there before it: the card view
// mounts only once the app has read its config, behind a first-run screen
// not at all, so a card asked for sooner waits for it here.
import { ref } from "vue";
import router, { MANAGE_STACK } from "@/router";
import { searchSource } from "@/cards/cardSources";

export const SOURCES_CARD = "sourcesView()";
export const LOG_CARD = "logView()";
export const SEARCH_CARD = "searchView()";

export type SurfaceCommands = {
  // A new gallery card, revealed.
  addCard(): void;
  // Reveal the card whose source is `source`; open one if none is showing.
  showCard(source: string): void;
};

export const surface = ref<SurfaceCommands | null>(null);

const waiting: string[] = [];

/// Called by CardsView as its layout mounts and unmounts. What was asked for
/// before it mounted is shown then.
export function registerSurface(commands: SurfaceCommands | null) {
  surface.value = commands;
  if (commands) for (const source of waiting.splice(0)) commands.showCard(source);
}

function reveal(source: string) {
  if (surface.value) surface.value.showCard(source);
  else waiting.push(source);
}

export function showDataSources() {
  if (surface.value) surface.value.showCard(SOURCES_CARD);
  else void router.push(MANAGE_STACK);
}

/// Show the card `source` names, opening it where none is: a chip in
/// the chrome double-clicked, or one of its menu's entries.
export function showCard(source: string) {
  reveal(source);
}

/// A search card of its own on `q`, beside whatever is
/// showing.
export function searchFor(q: string) {
  reveal(searchSource(q));
}
