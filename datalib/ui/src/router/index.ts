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
/// The card stack the Manage tab opens: the sources tree at 1.6× the
/// default column width, the config editor beside it.
export const MANAGE_STACK = encodeColumns([
  { code: "sourcesView()", size: 1.6, state: "" },
  { code: "configView()", size: null, state: "" },
]);

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/sources",
      name: "sources",
      component: () => import("@/views/SourcesView.vue"),
    },
    // Manager2 is two cards on the card surface: the sources tree, wide,
    // with the config editor beside it. The path stays so links and
    // muscle memory keep working.
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
