import { createRouter, createWebHashHistory } from "vue-router";

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
      component: () => import("@/views/SearchView.vue"),
    },
    {
      path: "/chat/:conversationUuid",
      name: "chat",
      component: () => import("@/views/ChatView.vue"),
    },
    {
      path: "/prefs",
      name: "prefs",
      component: () => import("@/views/PreferencesView.vue"),
    },
  ],
});

export default router;
