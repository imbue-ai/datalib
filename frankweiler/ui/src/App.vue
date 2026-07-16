<script setup lang="ts">
import { RouterView, RouterLink } from "vue-router";
import SyncProgressChrome from "@/components/SyncProgressChrome.vue";
import ToastStack from "@/components/ToastStack.vue";
</script>

<template>
  <main class="frankweiler-shell" data-feedback-root>
    <header class="fw-header">
      <h1>datalib</h1>
      <nav class="fw-tabs" aria-label="Navigation">
        <RouterLink class="fw-tab" to="/">Explore</RouterLink>
        <RouterLink class="fw-tab" to="/sources">Sources</RouterLink>
      </nav>
      <div class="fw-spacer" />
    </header>

    <SyncProgressChrome />
    <RouterView />
    <ToastStack />
  </main>
</template>

<style>
:root {
  color-scheme: light dark;
  --fw-bg: #ffffff;
  --fw-fg: #1a1a1a;
  --fw-muted: #6b6b6b;
  --fw-border: #d8d8d8;
  --fw-input-bg: #ffffff;
  --fw-code-bg: #f4f4f4;
  --fw-hover: #f0f0f0;
  --fw-accent: #2563eb;
  --fw-card-bg: #fafafa;
  /* Log severity highlights: dark shades on the light background… */
  --fw-log-error: #991b1b;
  --fw-log-warn: #854d0e;
}

@media (prefers-color-scheme: dark) {
  :root {
    --fw-bg: #1a1b1e;
    --fw-fg: #e6e6e6;
    --fw-muted: #9aa0a6;
    --fw-border: #2f3136;
    --fw-input-bg: #232428;
    --fw-code-bg: #2a2b2f;
    --fw-hover: #2a2b2f;
    --fw-accent: #6ea8fe;
    --fw-card-bg: #232428;
    /* …and light shades on the dark background. */
    --fw-log-error: #f87171;
    --fw-log-warn: #facc15;
  }
}

html,
body,
#app {
  background: var(--fw-bg);
  color: var(--fw-fg);
  margin: 0;
  min-height: 100vh;
}

body {
  font-family: system-ui, sans-serif;
}

a {
  color: var(--fw-accent);
}

.frankweiler-shell {
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
.fw-header {
  flex: 0 0 auto;
}

/* Browser-style tab band: the header sits on a tinted strip and the
   active tab is cut from the page background, flowing into the
   content below with no separating line under it. Negative margins
   counter the shell's 1rem padding so the band runs full-bleed. */
.fw-header {
  display: flex;
  align-items: flex-end;
  gap: 0.6rem;
  margin: -1rem -1rem 0.75rem;
  padding: 0.5rem 1rem 0;
  background: var(--fw-card-bg);
  border-bottom: 1px solid var(--fw-border);
}
.fw-header h1 {
  margin: 0 0 0.45rem 0;
  font-size: 1.25rem;
}
.fw-spacer {
  flex: 1;
}
.fw-tabs {
  display: flex;
  gap: 2px;
  margin-left: 0.75rem;
}
/* The active tab is cut from the page background; border-bottom: none
   plus the -1px overlap lets its background erase the band's hairline
   so tab and page read as one surface. */
.fw-tab {
  padding: 0.35rem 0.95rem;
  margin-bottom: -1px;
  border: 1px solid transparent;
  border-bottom: none;
  border-radius: 4px 4px 0 0;
  color: var(--fw-muted);
  text-decoration: none;
  line-height: 1.4;
}
.fw-tab:hover {
  background: var(--fw-hover);
  color: var(--fw-fg);
}
.fw-tab.router-link-active {
  background: var(--fw-bg);
  border-color: var(--fw-border);
  color: var(--fw-accent);
  font-weight: 600;
}
</style>
