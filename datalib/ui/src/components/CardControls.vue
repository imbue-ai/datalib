<script setup lang="ts">
// The common controls every card carries in its chrome bar, regardless
// of layout: the agent hand-off button (🤖, only on cards backed by a
// user component), the help popup (?), a link to open the card alone
// (↗), and the close button (✕). All are pure functions of the card's
// source and its CardCtx. Close goes through ctx.host.close() — the
// host command built for exactly this — so nothing here knows the
// layout.
import { computed, ref, watch } from "vue";
import { encodeColumns } from "@/router/columns";
import { modifyComponentWithAgent } from "@/handoff";
import { ensureFrontend, frontendManifest } from "@/cards/frontendRegistry";
import { cardHelp } from "@/cards/help";
import type { CardCtx } from "@/cards/types";

const props = defineProps<{
  source: string;
  ctx: CardCtx;
  // Drawn in a tab's sidebar row, which has its own pop-out and close:
  // only the hand-off and the help, then.
  sidebar?: boolean;
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

// Standalone view: a URL naming just this card at its current state,
// which a new window opens as a tab of its own.
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
  <a
    v-if="!sidebar && source.trim() !== ''"
    class="card-control card-control--alone"
    :href="aloneHref"
    target="_blank"
    rel="noopener"
    title="open this card alone, in a new tab or window"
    >↗</a
  >
  <button
    v-if="!sidebar"
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
  color: var(--datalib-muted);
  cursor: pointer;
  font-size: var(--datalib-font-size-small);
  line-height: 1.5;
  text-decoration: none;
  padding: 0.2rem 0;
}
.card-control:hover {
  color: var(--datalib-fg);
}
</style>

<style>
/* The help popup is teleported to <body>, outside this component's
   scope, so its styles are global. */
.card-help-backdrop {
  position: fixed;
  inset: 0;
  z-index: 1000;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  align-items: center;
  justify-content: center;
}
.card-help {
  width: min(720px, 90vw);
  max-height: 85vh;
  overflow: auto;
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 4px);
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.22);
  font-family: var(--datalib-font);
}
.card-help-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.card-help-head h3 {
  margin: 0;
  font-size: calc(var(--datalib-title-size) + 2px);
}
.card-help-close {
  padding: 2px 9px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-bg);
  color: inherit;
  font: inherit;
  font-size: var(--datalib-font-size-small);
  cursor: pointer;
}
.card-help-close:hover {
  background: var(--datalib-hover);
}
.card-help-body {
  padding: 4px 16px 16px;
  font-size: var(--datalib-font-size);
  line-height: 1.5;
}
.card-help-body p {
  margin: 10px 0;
}
.card-help-body code {
  font-family: var(--datalib-mono);
  font-size: var(--datalib-font-size-small);
}
</style>
