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
//
// History mode also requires the backend to fall back to `index.html`
// for unknown paths. The embedded server already does this — see
// `frankweiler/backend/http/src/embed.rs`'s `serve_ui` fallback —
// and Vite's dev server does it out of the box for SPAs.
const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/sources",
      name: "sources",
      component: () => import("@/views/SourcesView.vue"),
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
