<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { RouterView, RouterLink } from "vue-router";
import SyncProgressChrome from "@/components/SyncProgressChrome.vue";
import ToastStack from "@/components/ToastStack.vue";
import AgentHandoffModal from "@/components/AgentHandoffModal.vue";
import FirstRunView from "@/views/FirstRunView.vue";
import ConfigErrorView from "@/views/ConfigErrorView.vue";
import { fetchConfig, type ConfigResponse } from "@/api";
import { subscribeLive } from "@/live";

// The gate in front of the whole app, for the two states where showing
// the app would be a lie.
//
// Both come from one fact: `config.toml` declares the `unified_index`
// applet, and that applet *is* the grid, the search and the document
// view. Without a usable one, every view answers
// `502 {"error":"no applet \"unified_index\""}` — which was, literally,
// a new user's first impression (#199) and what a single stray config
// key did to a working install (#209).
//
//   * **No config at all** → the onboarding screen, which explains what
//     initializing will write before writing it.
//   * **A config the server can't run on** → the config-error screen.
//     `app_ready` is the backend's answer, false when the file is not a
//     config or when it declares no usable `unified_index` applet.
//
// A config with a merely *broken step* is neither of those. It loads,
// the app works, and that step's diagnostic belongs on its row in the
// Pipeline table. That distinction is the point of the graded loader:
// before it, one bad key landed you on a blocking screen.
//
// ## The gate is live, in both directions
//
// A config does not only break at startup. An agent editing the root, a
// text editor, a `git checkout` — any of them can break or fix the file
// while the app is open, and `watch.rs` reports every one of them as
// `config_changed`. So this re-checks on that event, and the direction
// that is easy to forget is the *second* one: a config that gets fixed
// has to drop the gate on its own, with no reload. `resync` covers the
// case where the stream itself dropped and we missed the frame.
//
// `null` while the first check is in flight — render nothing rather
// than flash a view that is about to be replaced. A failed check
// (backend blip, offline) falls through to the app: the gate exists to
// explain a broken root, not to become a third way for the app not to
// load.
const config = ref<ConfigResponse | null>(null);
const checked = ref(false);

const gate = computed<"first-run" | "config-error" | null>(() => {
  const c = config.value;
  if (!c) return null;
  if (!c.exists) return "first-run";
  return c.app_ready ? null : "config-error";
});

async function refresh() {
  try {
    config.value = await fetchConfig();
  } catch {
    config.value = null;
  } finally {
    checked.value = true;
  }
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
    <header class="datalib-header">
      <h1>datalib</h1>
      <nav v-if="!gate" class="datalib-tabs" aria-label="Navigation">
        <RouterLink class="datalib-tab" to="/">Explore</RouterLink>
        <RouterLink class="datalib-tab" to="/sources">Manage</RouterLink>
        <RouterLink class="datalib-tab" to="/sources2">Manager2</RouterLink>
      </nav>
      <div class="datalib-spacer" />
      <!-- Lightweight sync indicator in the header's flexible space —
           appearing/disappearing never shifts the page layout. -->
      <SyncProgressChrome />
    </header>

    <FirstRunView
      v-if="gate === 'first-run' && config"
      :config="config"
      @initialized="onInitialized"
    />
    <ConfigErrorView v-else-if="gate === 'config-error' && config" :config="config" />
    <RouterView v-else-if="checked" />
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
