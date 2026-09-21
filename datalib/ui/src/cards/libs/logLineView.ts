// `logLineView(seq)` in card source: one log line in full (see
// cards/LogLineCard.ce.vue), opened beside the log by a selection in
// it — a different line is a different card, as a document is for the
// grid.
import LogLineCard from "../LogLineCard.ce.vue";
import JsonTree from "../JsonTree.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function logLineView(seq: number): CardRender {
  return vueCard(LogLineCard, { seq }, { styleSources: [JsonTree] });
}

export function logLineSource(seq: number): string {
  return `logLineView(${seq})`;
}
