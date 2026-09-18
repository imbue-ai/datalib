import { encodeColumns } from "@/router/columns";
import { createRouter, createWebHistory } from "vue-router";

// History-mode routing: the URL path *is* the Miller column stack —
// each path segment encodes one column as `code:state` (see
// `router/columns.ts`). `/` is an empty stack; the empty-stack case is
// rendered as the default `[gridView()]` by `MillerView`. The routed
// component is `CardsView`, which hosts MillerView plus the
// URL-independent tree layout behind a toggle.
//
// The catchall MUST come after the explicit routes (`/sources` and the
// legacy redirects); Vue Router does prefer specific over param routes by
// path-rank, but order is the simpler invariant.
/// The card stack the Manager2 tab opens, and where a just-initialized
/// library lands: the sources tree alone, at 1.6× the default column
/// width. The config editor is a click away from that card, not open
/// beside it — the first thing to do on the screen is add a source.
export const MANAGE_STACK = encodeColumns([{ code: "sourcesView()", size: 1.6, state: "" }]);

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/sources",
      name: "sources",
      component: () => import("@/views/SourcesView.vue"),
    },
    // Manager2 is the sources card on the card surface. The path stays
    // so links, muscle memory and the launch URL keep working.
    { path: "/sources2", redirect: MANAGE_STACK },
    // The old Setup and Sync tabs merged into Sources; keep the paths
    // working for muscle memory and stale links.
    { path: "/setup", redirect: "/sources" },
    { path: "/sync", redirect: "/sources" },
    {
      path: "/:stack(.*)*",
      name: "cards",
      component: () => import("@/views/CardsView.vue"),
    },
  ],
});

export default router;
