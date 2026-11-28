// `contactsView()` in card source: every contact in the contacts app, read
// straight from it; `contactsView({ label })`, those carrying one label,
// as the Library tree shows a label. See cards/ContactsCard.ce.vue.
import ContactsCard from "../ContactsCard.ce.vue";
import SuggestionItem from "../SuggestionItem.ce.vue";
import SuggestionsList from "../SuggestionsList.ce.vue";
import chipCss from "../chip.css?inline";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function contactsView(opts: { label?: unknown } = {}): CardRender {
  const label = opts?.label ?? null;
  if (label !== null && typeof label !== "string") {
    throw new Error('contactsView takes a label as text: contactsView({ label: "family" })');
  }
  return vueCard(
    ContactsCard,
    { label },
    { styleSources: [SuggestionItem, SuggestionsList, slickCss, tableGridCss, chipCss] },
  );
}
