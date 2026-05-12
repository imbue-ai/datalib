<script setup lang="ts">
import { RouterView } from "vue-router";
</script>

<template>
  <main class="frankweiler-shell" data-feedback-root>
    <header><h1>Frankweiler</h1></header>
    <RouterView />
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
  padding: 1rem;
}

.frankweiler-shell header h1 {
  margin: 0 0 0.75rem 0;
  font-size: 1.25rem;
}
</style>
