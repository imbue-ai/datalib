<script setup lang="ts">
// The common controls every card carries in its chrome bar, regardless
// of layout: the agent hand-off button (🤖, only on cards backed by a
// user component), back / forward over the card's own source history
// (← →), a link to open the card alone (↗), and the close button (✕).
// All are pure functions of the card's source and its CardCtx, so the
// layouts (miller, tiling, tree) all render this same component instead
// of duplicating the markup and CSS. Close goes through
// ctx.host.close() — the host command built for exactly this — so
// nothing here knows the layout.
import { computed, ref, watch } from "vue";
import { encodeColumns } from "@/router/columns";
import { modifyComponentWithAgent } from "@/handoff";
import { ensureFrontend, frontendManifest } from "@/cards/frontendRegistry";
import type { CardCtx } from "@/cards/types";

const props = defineProps<{
  source: string;
  ctx: CardCtx;
}>();

// ---- agent hand-off (🤖) ----
//
// Shown only for a card calling a component in the `user` namespace.
// That is the only namespace an agent can usefully edit: an applet's
// namespace is deleted and rewritten on every refresh, so an edit there
// would vanish the next time the config is touched. Builtins never
// match either — they live in the app bundle, not in the store.
//
// Detected from the source's leading `comp.user.<name>(` against the
// reactive manifest, so the button appears the moment the store loads
// (idempotent kick below) and follows renames and deletes.
void ensureFrontend();
const aliasName = computed(() => {
  const m = props.source.match(/^\s*comp\s*\.\s*user\s*\.\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*\(/);
  if (!m) return null;
  return frontendManifest.value.get("user")?.has(m[1]) ? m[1] : null;
});

function handOff() {
  if (!aliasName.value) return;
  modifyComponentWithAgent(aliasName.value, props.source, props.ctx.initialState);
}

// Standalone view: a miller URL containing just this card, at its
// current state (initialState is a live getter in every layout).
const aloneHref = computed(() =>
  encodeColumns([{ code: props.source, state: props.ctx.initialState }]),
);

// ---- back / forward over the card's own source history ----
//
// A card can navigate in place — the gallery becomes a picker becomes
// a document (host.setSource), the agent hand-off repoints it, a dev
// edits the source box. Each is a step in this card's history, tracked
// here as the classic stack + cursor: a new source truncates any
// forward entries and appends; back/forward move the cursor and replay
// the entry through host.setSource. Our own replay comes back as a
// prop change that matches the cursor, which the watcher ignores.
// History is per chrome-bar instance, so it lives exactly as long as
// the card does in its layout (source only — state is cleared by
// setSource, so navigating restores a fresh card, like a page reload).
const history = ref<string[]>([props.source]);
const cursor = ref(0);

watch(
  () => props.source,
  (next) => {
    if (next === history.value[cursor.value]) return;
    history.value = [...history.value.slice(0, cursor.value + 1), next];
    cursor.value = history.value.length - 1;
  },
);

const canBack = computed(() => cursor.value > 0);
const canForward = computed(() => cursor.value < history.value.length - 1);

function goBack() {
  if (!canBack.value) return;
  cursor.value--;
  props.ctx.host.setSource(history.value[cursor.value]);
}

function goForward() {
  if (!canForward.value) return;
  cursor.value++;
  props.ctx.host.setSource(history.value[cursor.value]);
}
</script>

<template>
  <!-- The agent hand-off, for cards backed by a user component. First
       of the controls so its coming and going doesn't move the rest,
       which stay pinned to the bar's right edge. -->
  <button
    v-if="aliasName"
    class="card-control card-control--agent"
    title="let a coding agent modify this card's component"
    @click="handOff"
  >
    🤖
  </button>
  <button
    class="card-control card-control--back"
    :disabled="!canBack"
    title="back"
    @click="goBack"
  >
    ←
  </button>
  <button
    class="card-control card-control--forward"
    :disabled="!canForward"
    title="forward"
    @click="goForward"
  >
    →
  </button>
  <a
    v-if="source.trim() !== ''"
    class="card-control card-control--alone"
    :href="aloneHref"
    target="_blank"
    rel="noopener"
    title="open this card alone"
    >↗</a
  >
  <button
    class="card-control card-control--close"
    title="close card"
    @click="ctx.host.close()"
  >
    ✕
  </button>
</template>

<style scoped>
/* The controls sit directly in the layout's chrome bar (a flex row).
   No wrapping element — the component's template renders the controls
   as siblings — so they keep the bar's existing gap and alignment. */
.card-control {
  flex: 0 0 auto;
  border: none;
  background: transparent;
  color: inherit;
  opacity: 0.6;
  cursor: pointer;
  font-size: 0.8rem;
  line-height: 1.5;
  text-decoration: none;
  padding: 0.2rem 0;
}
.card-control:hover {
  opacity: 1;
}
/* Kept visible-but-dim (not hidden) when there's nowhere to go, so
   the bar doesn't reflow as history accrues. */
.card-control:disabled {
  opacity: 0.2;
  cursor: default;
}
/* The arrow glyphs render smaller than the other icons at the shared
   size — bump the font, and pin the line box to the shared 1.2rem
   (0.8rem × 1.5) so the bar height doesn't change. */
.card-control--back,
.card-control--forward {
  font-size: 0.95rem;
  line-height: 1.2rem;
}
</style>
