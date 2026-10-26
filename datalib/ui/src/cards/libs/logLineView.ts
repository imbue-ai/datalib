// `logLineView(seq)` in card source: one log line in full (see
// cards/LogLineCard.ce.vue), opened beside the log by a selection in
// it — a different line is a different card, as a document is for the
// grid.
import LogLineCard from "../LogLineCard.ce.vue";
import JsonTree from "../JsonTree.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import EntityChip from "../EntityChip.ce.vue";
import chipCss from "../chip.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function logLineView(seq: number): CardRender {
  if (!Number.isSafeInteger(seq) || seq < 0) {
    throw new Error("logLineView takes a log line's number, as in logLineView(42)");
  }
  return vueCard(LogLineCard, { seq }, { styleSources: [JsonTree, EntityChip, ChipMenu, chipCss] });
}

export function logLineSource(seq: number): string {
  return `logLineView(${seq})`;
}
