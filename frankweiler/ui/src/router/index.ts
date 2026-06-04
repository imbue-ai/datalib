import { createRouter, createWebHashHistory } from "vue-router";

// Both `/search` and `/chat/:markdownUuid` are served by the same
// MillerView. They differ only in what the *initial* column stack
// looks like: `search` starts with `[grid]`, `chat` starts with one
// `doc:<uuid>` column. The view's column array is what carries
// everything else (deeper docs pushed by inner link clicks).
const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    {
      path: "/",
      redirect: "/search",
    },
    {
      path: "/search",
      name: "search",
      component: () => import("@/views/MillerView.vue"),
    },
    {
      path: "/chat/:markdownUuid",
      name: "chat",
      component: () => import("@/views/MillerView.vue"),
    },
    {
      path: "/sync",
      name: "sync",
      component: () => import("@/views/SyncView.vue"),
    },
    {
      path: "/prefs",
      name: "prefs",
      component: () => import("@/views/PreferencesView.vue"),
    },
  ],
});

export default router;
