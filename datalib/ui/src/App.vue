<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { RouterView } from "vue-router";
import SyncProgressChrome from "@/components/SyncProgressChrome.vue";
import ToastStack from "@/components/ToastStack.vue";
import AgentHandoffModal from "@/components/AgentHandoffModal.vue";
import FirstRunView from "@/views/FirstRunView.vue";
import ConfigErrorView from "@/views/ConfigErrorView.vue";
import NewerRootView from "@/views/NewerRootView.vue";
import UpgradingView from "@/views/UpgradingView.vue";
import RerenderDialog from "@/components/RerenderDialog.vue";
import { fetchConfig, openRequest, type ConfigResponse } from "@/api";
import { newestAnswer, subscribeLive } from "@/live";
import CommandBox from "@/components/CommandBox.vue";
import LibraryCrumb from "@/components/LibraryCrumb.vue";
import { isDesktopApp } from "@/desktop";

// The app window has no browser chrome, so it draws the two buttons a
// tab would have; the browser keeps its own.
const desktop = isDesktopApp();
// On macOS the desktop app's page runs under the title bar (the Tauri
// shell's `under_title_bar`): the toolbar is the title bar, so it leaves
// the window buttons room and its empty areas move the window.
const underTitleBar = desktop && /Mac/.test(navigator.platform);

// The gate in front of the whole app, for the states where showing the
// app would be a lie.
const config = ref<ConfigResponse | null>(null);
const checked = ref(false);

const gate = computed<"first-run" | "newer-root" | "upgrading" | "config-error" | null>(() => {
  const c = config.value;
  if (!c) return null;
  // A refused root comes first: with no store open, "no config" and
  // "not ready" are both consequences of it, not states of their own.
  if (c.newer_root) return "newer-root";
  if (c.upgrade.migrating) return "upgrading";
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
/// newer "ready" would put the gate up over a config that is fine. One
/// fetch at a time: a launch's migrate pass sends an `upgrade_changed`
/// per step, milliseconds apart.
const refresh = newestAnswer(
  () => fetchConfig().catch(() => null),
  (next) => {
    config.value = next;
    checked.value = true;
  },
);

// Initializing just wrote the config, so drop the gate on the click
// rather than on the round trip after it — `refresh` then replaces this
// guess with the truth, and `config_changed` would have anyway.
function onInitialized() {
  if (config.value) config.value = { ...config.value, exists: true, app_ready: true };
  refresh();
}

// The launch's re-render offer, asked once per page: a launch of the
// desktop app is a page load, and the next launch asks again while the
// steps have not run. A step the pass could not ask does not open it on
// its own: that is a failed run on the step's row and in its log.
const rerenderAnswered = ref(false);
const rerenderAsked = computed(() => {
  const u = config.value?.upgrade;
  if (!u || gate.value || rerenderAnswered.value) return false;
  return u.rerender.length > 0;
});

async function onRerender(yes: boolean) {
  rerenderAnswered.value = true;
  if (yes && config.value) await openRequest(config.value.upgrade.rerender, "upgrade");
}

let stop: (() => void) | null = null;
onMounted(() => {
  refresh();
  stop = subscribeLive({
    root: (e) => {
      if (e.kind === "config_changed" || e.kind === "upgrade_changed") refresh();
    },
    resync: refresh,
  });
});
onUnmounted(() => stop?.());
</script>

<template>
  <main class="datalib-shell" data-feedback-root>
    <!-- The toolbar: the app and library names, and the search box. The
         first-run screen keeps the names, which are the way back to the
         other libraries; there is nothing yet to sync or search. -->
    <nav
      v-if="!gate || gate === 'first-run'"
      class="datalib-toolbar"
      :class="{ 'datalib-toolbar--titlebar': underTitleBar }"
      aria-label="App"
      data-tauri-drag-region
    >
      <div class="datalib-toolbar-start" data-tauri-drag-region>
        <LibraryCrumb :config-path="config?.path ?? null" />
      </div>
      <template v-if="!gate">
        <div class="datalib-toolbar-sync" data-tauri-drag-region><SyncProgressChrome /></div>
        <div class="datalib-toolbar-search"><CommandBox /></div>
      </template>
    </nav>

    <!-- The gates had the shell's padding before the cards went
         edge to edge; they keep it here. -->
    <div v-if="gate" class="datalib-gate">
      <FirstRunView
        v-if="gate === 'first-run' && config"
        :config="config"
        @initialized="onInitialized"
      />
      <NewerRootView v-else-if="gate === 'newer-root' && config" :config="config" />
      <UpgradingView v-else-if="gate === 'upgrading' && config" :config="config" />
      <ConfigErrorView
        v-else-if="gate === 'config-error' && config"
        :config="config"
        @saved="refresh"
      />
    </div>
    <div v-if="cardsShown" v-show="!gate" class="datalib-cards">
      <RouterView />
    </div>
    <RerenderDialog v-if="rerenderAsked && config" :upgrade="config.upgrade" @answer="onRerender" />
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

.datalib-shell {
  /* Viewport-pinned flex column: the toolbar takes its natural height
     and the routed view flexes into the rest, so full-height views
     (the card layout) reach the bottom without guessing the chrome height.
     min-height (not height) so taller views (sync) still
     scroll the page normally. */
  display: flex;
  flex-direction: column;
  min-height: 100vh;
  box-sizing: border-box;
}
.datalib-gate {
  flex: 1 1 auto;
  display: flex;
  flex-direction: column;
  padding: 1rem;
}
.datalib-toolbar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 4px;
  height: var(--datalib-toolbar-h);
  box-sizing: border-box;
  padding: 0 10px;
  background: var(--datalib-ground);
  border-bottom: 1px solid var(--datalib-border);
}
/* The title bar's height, whatever the density: the window buttons are
   placed once, when the window opens, at this bar's middle. So its
   controls keep their step-0 height too; taller ones crowd a 40px bar. */
.datalib-toolbar--titlebar {
  --datalib-control-h: 24px;
  height: 40px;
  padding-left: 92px;
  -webkit-user-select: none;
  user-select: none;
}
/* The search box at the right end. As the window narrows the search
   box shrinks first, from 440px to its min-width (its far larger
   flex-shrink leaves it nearly all the shrinking); past that the
   crumb's library name ellipsizes. The desktop shell's minimum window
   width (MIN_WINDOW_WIDTH in datalib/tauri/src/main.rs) keeps both in
   view. */
.datalib-toolbar-start {
  flex: 1 1 auto;
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
}
.datalib-toolbar-sync {
  flex: 0 0 auto;
  display: flex;
}
.datalib-toolbar-search {
  flex: 0 1000 440px;
  min-width: 180px;
  display: flex;
}
</style>
