// `documentView("md-uuid", "section-uuid")` in card source returns a
// CardRender for the document card (see cards/DocCard.ce.vue), which
// renders that doc with the given section highlighted. A different
// selection is a different card — the grid opens a fresh column.
import DocCard from "../DocCard.ce.vue";
import ChatBody from "../ChatBody.ce.vue";
import FeedbackButton from "@/components/FeedbackButton.ce.vue";
// Inside a shadow root we have to inject stylesheets ourselves —
// the document-head styles Vite would normally produce don't pierce
// the shadow boundary. `?inline` asks Vite for the stylesheet text.
import hljsCss from "highlight.js/styles/github-dark.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function documentView(
  markdownUuid?: string | null,
  sectionUuid?: string | null,
): CardRender {
  return vueCard(
    DocCard,
    {
      markdownUuid: markdownUuid ?? null,
      sectionUuid: sectionUuid ?? null,
    },
    { styleSources: [ChatBody, FeedbackButton, hljsCss] },
  );
}
