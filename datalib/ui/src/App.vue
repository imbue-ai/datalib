<script setup lang="ts">
import { RouterView, RouterLink } from "vue-router";
import SyncProgressChrome from "@/components/SyncProgressChrome.vue";
import ToastStack from "@/components/ToastStack.vue";
import AgentHandoffModal from "@/components/AgentHandoffModal.vue";
</script>

<template>
  <main class="datalib-shell" data-feedback-root>
    <header class="datalib-header">
      <h1>datalib</h1>
      <nav class="datalib-tabs" aria-label="Navigation">
        <RouterLink class="datalib-tab" to="/">Explore</RouterLink>
        <RouterLink class="datalib-tab" to="/sources">Manage</RouterLink>
      </nav>
      <div class="datalib-spacer" />
      <!-- Lightweight sync indicator in the header's flexible space —
           appearing/disappearing never shifts the page layout. -->
      <SyncProgressChrome />
    </header>

    <RouterView />
    <ToastStack />
    <!-- Agent hand-off instructions dialog; opened via handoff.ts from
         the card surface and the Manage tab's config editor. -->
    <AgentHandoffModal />
  </main>
</template>

<style>
:root {
  color-scheme: light dark;
  --datalib-bg: #ffffff;
  --datalib-fg: #1a1a1a;
  --datalib-muted: #6b6b6b;
  --datalib-border: #d8d8d8;
  --datalib-input-bg: #ffffff;
  --datalib-code-bg: #f4f4f4;
  --datalib-hover: #f0f0f0;
  --datalib-accent: #2563eb;
  --datalib-card-bg: #fafafa;
  /* Log severity highlights: dark shades on the light background… */
  --datalib-log-error: #991b1b;
  --datalib-log-warn: #854d0e;
}

@media (prefers-color-scheme: dark) {
  :root {
    --datalib-bg: #1a1b1e;
    --datalib-fg: #e6e6e6;
    --datalib-muted: #9aa0a6;
    --datalib-border: #2f3136;
    --datalib-input-bg: #232428;
    --datalib-code-bg: #2a2b2f;
    --datalib-hover: #2a2b2f;
    --datalib-accent: #6ea8fe;
    --datalib-card-bg: #232428;
    /* …and light shades on the dark background. */
    --datalib-log-error: #f87171;
    --datalib-log-warn: #facc15;
  }
}

html,
body,
#app {
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  margin: 0;
  min-height: 100vh;
}

body {
  font-family: system-ui, sans-serif;
}

a {
  color: var(--datalib-accent);
}

.datalib-shell {
  /* Viewport-pinned flex column: the header takes its natural height
     and the routed view flexes into the rest, so full-height views
     (MillerView) reach the bottom without guessing the chrome height.
     min-height (not height) so taller views (sync) still
     scroll the page normally. */
  display: flex;
  flex-direction: column;
  min-height: 100vh;
  box-sizing: border-box;
  padding: 1rem;
}
.datalib-header {
  flex: 0 0 auto;
}

/* Browser-style tab band: the header sits on a tinted strip and the
   active tab is cut from the page background, flowing into the
   content below with no separating line under it. Negative margins
   counter the shell's 1rem padding so the band runs full-bleed. */
.datalib-header {
  display: flex;
  align-items: flex-end;
  gap: 0.6rem;
  margin: -1rem -1rem 0.75rem;
  padding: 0.5rem 1rem 0;
  background: var(--datalib-card-bg);
  border-bottom: 1px solid var(--datalib-border);
}
.datalib-header h1 {
  margin: 0 0 0.45rem 0;
  font-size: 1.25rem;
}
.datalib-spacer {
  flex: 1;
}
.datalib-tabs {
  display: flex;
  gap: 2px;
  margin-left: 0.75rem;
}
/* The active tab is cut from the page background; border-bottom: none
   plus the -1px overlap lets its background erase the band's hairline
   so tab and page read as one surface. */
.datalib-tab {
  padding: 0.35rem 0.95rem;
  margin-bottom: -1px;
  border: 1px solid transparent;
  border-bottom: none;
  border-radius: 4px 4px 0 0;
  color: var(--datalib-muted);
  text-decoration: none;
  line-height: 1.4;
}
.datalib-tab:hover {
  background: var(--datalib-hover);
  color: var(--datalib-fg);
}
.datalib-tab.router-link-active {
  background: var(--datalib-bg);
  border-color: var(--datalib-border);
  color: var(--datalib-accent);
  font-weight: 600;
}
</style>
