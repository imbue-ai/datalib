<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { RouterView } from "vue-router";
import SyncProgressChrome from "@/components/SyncProgressChrome.vue";
import ToastStack from "@/components/ToastStack.vue";
import AgentHandoffModal from "@/components/AgentHandoffModal.vue";
import FirstRunView from "@/views/FirstRunView.vue";
import ConfigErrorView from "@/views/ConfigErrorView.vue";
import NewerRootView from "@/views/NewerRootView.vue";
import { fetchConfig, type ConfigResponse } from "@/api";
import { subscribeLive } from "@/live";
import { newCard, showDataSources } from "@/surface";
import { isDesktopApp } from "@/desktop";

// The app window has no browser chrome, so it draws the two buttons a
// tab would have; the browser keeps its own.
const desktop = isDesktopApp();
const goBack = () => history.back();
const goForward = () => history.forward();

// The gate in front of the whole app, for the three states where showing
// the app would be a lie.
const config = ref<ConfigResponse | null>(null);
const checked = ref(false);

const gate = computed<"first-run" | "newer-root" | "config-error" | null>(() => {
  const c = config.value;
  if (!c) return null;
  // A refused root comes first: with no store open, "no config" and
  // "not ready" are both consequences of it, not states of their own.
  if (c.newer_root) return "newer-root";
  if (!c.exists) return "first-run";
  return c.app_ready ? null : "config-error";
});

/// Set once the cards have been shown. From then on the gate hides
/// them rather than unmounting them: a config broken for a moment (an
/// agent's write caught half done, a `git checkout`) must not cost every
/// open card its scroll, selection and query.
const cardsShown = ref(false);
watch(
  () => checked.value && !gate.value,
  (open) => {
    if (open) cardsShown.value = true;
  },
  { immediate: true },
);

/// Only the newest answer is kept: an older "not ready" landing after a
/// newer "ready" would put the gate up over a config that is fine.
let asked = 0;
async function refresh() {
  const mine = ++asked;
  let next: ConfigResponse | null = null;
  try {
    next = await fetchConfig();
  } catch {
    next = null;
  }
  if (mine !== asked) return;
  config.value = next;
  checked.value = true;
}

// Initializing just wrote the config, so drop the gate on the click
// rather than on the round trip after it — `refresh` then replaces this
// guess with the truth, and `config_changed` would have anyway.
function onInitialized() {
  if (config.value) config.value = { ...config.value, exists: true, app_ready: true };
  void refresh();
}

let stop: (() => void) | null = null;
onMounted(() => {
  void refresh();
  stop = subscribeLive({
    root: (e) => {
      if (e.kind === "config_changed") void refresh();
    },
    resync: () => void refresh(),
  });
});
onUnmounted(() => stop?.());
</script>

<template>
  <main class="datalib-shell" data-feedback-root>
    <!-- The toolbar: the two places a person starts from. The old
         Manage screen is hidden, not gone — `/sources` still serves
         SourcesView.vue; a link here brings it back. -->
    <nav v-if="!gate" class="datalib-toolbar" aria-label="Cards">
      <template v-if="desktop">
        <button class="datalib-tool" title="back (⌘[)" @click="goBack">←</button>
        <button class="datalib-tool" title="forward (⌘])" @click="goForward">→</button>
      </template>
      <button class="datalib-tool" @click="showDataSources">Data sources</button>
      <button class="datalib-tool" @click="newCard">＋ New card</button>
      <div class="datalib-spacer" />
      <!-- Lightweight sync indicator in the toolbar's flexible space —
           appearing/disappearing never shifts the page layout. -->
      <SyncProgressChrome />
    </nav>

    <FirstRunView
      v-if="gate === 'first-run' && config"
      :config="config"
      @initialized="onInitialized"
    />
    <NewerRootView v-else-if="gate === 'newer-root' && config" :config="config" />
    <ConfigErrorView v-else-if="gate === 'config-error' && config" :config="config" />
    <div v-if="cardsShown" v-show="!gate" class="datalib-cards">
      <RouterView />
    </div>
    <ToastStack />
    <!-- Agent hand-off instructions dialog; opened via handoff.ts from
         the card surface and the config editor. -->
    <AgentHandoffModal />
  </main>
</template>

<style>
/* Only there to be hidden behind the gate; lays nothing out itself. */
.datalib-cards {
  display: contents;
}

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
  --datalib-log-ok: #166534;
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
    --datalib-log-ok: #4ade80;
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
  /* Viewport-pinned flex column: the toolbar takes its natural height
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
/* A band across the top: tinted, hairline below, full-bleed by
   countering the shell's 1rem padding. */
.datalib-toolbar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 0.4rem;
  margin: -1rem -1rem 0.75rem;
  padding: 0.4rem 1rem;
  background: var(--datalib-card-bg);
  border-bottom: 1px solid var(--datalib-border);
}
.datalib-spacer {
  flex: 1;
}
.datalib-tool {
  padding: 0.25rem 0.75rem;
  border: 1px solid transparent;
  border-radius: 4px;
  background: transparent;
  color: var(--datalib-fg);
  font: inherit;
  line-height: 1.4;
  cursor: pointer;
}
.datalib-tool:hover {
  background: var(--datalib-hover);
  border-color: var(--datalib-border);
}
</style>
