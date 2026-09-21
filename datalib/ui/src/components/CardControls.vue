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
import { cardHelp } from "@/cards/help";
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

// ---- help (?) ----
//
// Shown only when the card offered some via ctx.setHelp. The popup is
// teleported out of the layout, so it sits over everything and takes
// the page's own styles rather than the card's.
const help = cardHelp(props.ctx.cardId);
const helpOpen = ref(false);
function onHelpKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") helpOpen.value = false;
}
watch(helpOpen, (open) => {
  if (open) window.addEventListener("keydown", onHelpKeydown);
  else window.removeEventListener("keydown", onHelpKeydown);
});

// ---- back / forward over the card's own source history ----
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
    v-if="help"
    class="card-control card-control--help"
    title="what this card shows, and how to work it"
    :aria-expanded="helpOpen"
    @click="helpOpen = !helpOpen"
  >
    ?
  </button>
  <Teleport to="body">
    <div v-if="helpOpen && help" class="card-help-backdrop" @click.self="helpOpen = false">
      <div class="card-help" role="dialog" aria-modal="true" aria-label="About this card">
        <header class="card-help-head">
          <h3>About this card</h3>
          <button class="card-help-close" @click="helpOpen = false">Close</button>
        </header>
        <div class="card-help-body" v-html="help" />
      </div>
    </div>
  </Teleport>
  <button class="card-control card-control--back" :disabled="!canBack" title="back" @click="goBack">
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
    title="open this card alone, in a new tab or window"
    >↗</a
  >
  <button class="card-control card-control--close" title="close card" @click="ctx.host.close()">
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

<style>
/* The help popup is teleported to <body>, outside this component's
   scope, so its styles are global. */
.card-help-backdrop {
  position: fixed;
  inset: 0;
  z-index: 1000;
  background: color-mix(in srgb, var(--datalib-bg) 60%, transparent);
  display: flex;
  align-items: center;
  justify-content: center;
}
.card-help {
  width: min(720px, 90vw);
  max-height: 85vh;
  overflow: auto;
  background: var(--datalib-card-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: 8px;
  box-shadow: 0 12px 40px rgba(0, 0, 0, 0.35);
}
.card-help-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--datalib-border);
}
.card-help-head h3 {
  margin: 0;
  font-size: 15px;
}
.card-help-close {
  padding: 2px 9px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-card-bg);
  color: inherit;
  font: inherit;
  font-size: 12px;
  cursor: pointer;
}
.card-help-body {
  padding: 4px 16px 16px;
  font-size: 13px;
  line-height: 1.5;
}
.card-help-body p {
  margin: 10px 0;
}
.card-help-body code {
  font-size: 12px;
}
</style>
