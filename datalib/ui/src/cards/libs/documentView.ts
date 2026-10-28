// `documentView("md-uuid", "section-uuid")` in card source returns a
// CardRender for the document card (see cards/DocCard.ce.vue), which
// renders that doc with the given section highlighted. A different
// selection is a different card — the grid opens a fresh column.
import DocCard from "../DocCard.ce.vue";
import ChatBody from "../ChatBody.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import DocTitleBar from "../DocTitleBar.ce.vue";
import HandlePopover from "../HandlePopover.ce.vue";
import chipCss from "../chip.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

const STYLES = { styleSources: [ChatBody, ChipMenu, DocTitleBar, HandlePopover, chipCss] };

export function documentView(
  markdownUuid?: string | null,
  sectionUuid?: string | null,
): CardRender {
  // The card titles itself in DocCard via ctx.setTitle: "Document"
  // while loading (the uuid means nothing to a human), then the
  // document's actual name once the fetch lands.
  return vueCard(
    DocCard,
    { markdownUuid: markdownUuid ?? null, sectionUuid: sectionUuid ?? null },
    STYLES,
  );
}

/** The document read beside a list (the Search card's preview): its
 *  title bar offers to open it as a card, and titles it by `people`
 *  when the list does. Not a card source: nothing but a list makes one. */
export function documentPreview(
  markdownUuid: string,
  sectionUuid: string | null,
  people: { handle: string; shownAs: string }[],
): CardRender {
  return vueCard(DocCard, { markdownUuid, sectionUuid, preview: true, people }, STYLES);
}
