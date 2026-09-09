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
const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/sources",
      name: "sources",
      component: () => import("@/views/SourcesView.vue"),
    },
    // Manager2: the sources-grid rewrite of the Manage tab, alongside
    // the original while it's proven out. See docs/dev/plans/source_wizard.md.
    {
      path: "/sources2",
      name: "sources2",
      component: () => import("@/views/Manager2View.vue"),
    },
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
