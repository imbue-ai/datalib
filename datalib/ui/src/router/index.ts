import { chatUuidFromHref } from "@/cards/chatLink";
import { encodeColumns } from "@/router/columns";
import { createRouter, createWebHistory } from "vue-router";

// History-mode routing: the URL path *is* the Miller column stack —
// each path segment encodes one column as `code:state` (see
// `router/columns.ts`). `/` is an empty stack; the empty-stack case is
// rendered as the default `[gridView()]` by `MillerView`. The routed
// component is `CardsView`, which hosts MillerView plus the
// URL-independent tree layout behind a toggle.
//
// The browser's history is the only history the app keeps. Read
// docs/dev/cards.md § "The miller layout and the browser" before adding
// another.
//
// The catchall MUST come after the explicit routes (the
// `/data_sources` redirect); Vue Router does prefer specific over param routes by
// path-rank, but order is the simpler invariant.
/// The card stack `/data_sources` opens, and where a just-initialized
/// library lands: the sources tree alone, at 1.6× the default column
/// width. The config editor is a click away from that card, not open
/// beside it — the first thing to do on the screen is add a source.
export const MANAGE_STACK = encodeColumns([{ code: "sourcesView()", size: 1.6, state: "" }]);
/// The new-card gallery alone: where the toolbar's "New card" lands
/// when no card surface is showing.
export const NEW_CARD_STACK = encodeColumns([{ code: "galleryView()", size: null, state: "" }]);

const router = createRouter({
  history: createWebHistory(),
  routes: [
    // The sources card on the card surface. The path stays so links,
    // muscle memory and the launch URL keep working.
    { path: "/data_sources", redirect: MANAGE_STACK },
    {
      path: "/:stack(.*)*",
      name: "cards",
      component: () => import("@/views/CardsView.vue"),
    },
  ],
});

// The link every renderer writes into a document body — `/chat/<uuid>`,
// with or without a `#/` in front — is a page of its own: the document
// alone. A plain click never gets here (the document card opens the
// target beside itself); a new tab, a bookmark or a pasted link does.
export function documentStack(markdownUuid: string): string {
  return encodeColumns([
    { code: `documentView(${JSON.stringify(markdownUuid)})`, size: null, state: "" },
  ]);
}

router.beforeEach((to) => {
  const uuid = chatUuidFromHref(to.fullPath);
  return uuid ? documentStack(uuid) : true;
});

export default router;
