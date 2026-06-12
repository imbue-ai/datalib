<script setup lang="ts">
// Routed view for the card surface. Owns the chrome both layouts
// share — the bottom status bar and the layout toggle — and keeps
// each layout host alive across toggles (v-show, not v-if) so
// switching back doesn't lose its cards.
//
// The two layouts are deliberately independent: the miller layout
// syncs its column stack with the URL (see MillerView), the tree
// layout is in-memory only, and cards are NOT carried across when
// toggling. The tree host is mounted lazily on first use so the
// default columns experience doesn't pay for a hidden grid card.
import { onMounted, ref } from "vue";
import MillerView from "@/views/MillerView.vue";
import TreeView from "@/views/TreeView.vue";
import { fetchHealth, fetchSearch, type Health } from "@/api";

type Layout = "columns" | "tree";
const layout = ref<Layout>("columns");
const treeMounted = ref(false);

function setLayout(next: Layout) {
  layout.value = next;
  if (next === "tree") treeMounted.value = true;
}

// Backend status for the bottom status bar. Global (host-level) on
// purpose: it describes the backend and its data root, not any one
// card's query — with several grid cards it would otherwise repeat
// per card.
const health = ref<Health | null>(null);
const indexedTotal = ref<number | null>(null);
const healthError = ref<string | null>(null);
onMounted(async () => {
  try {
    health.value = await fetchHealth();
    // The backend's total_estimated is capped by the limit, so ask
    // with a large limit to get the real index size.
    indexedTotal.value = (await fetchSearch("", 100_000)).total_estimated;
  } catch (e) {
    healthError.value = (e as Error).message;
  }
});
</script>

<template>
  <div class="cards-root">
    <MillerView v-show="layout === 'columns'" />
    <TreeView v-if="treeMounted" v-show="layout === 'tree'" />
    <div class="cards-statusbar">
      <span v-if="healthError" class="cards-health--warn">
        backend unreachable: {{ healthError }}
      </span>
      <span v-else-if="health">
        backend ok<template v-if="indexedTotal != null">
          · {{ indexedTotal }} conversations indexed</template
        >
        under <code>{{ health.root }}</code>
        <span v-if="!health.root_exists" class="cards-health--warn">
          (root does not exist)</span
        >
      </span>
      <span class="cards-statusbar-spacer" />
      <div class="cards-layout-toggle" role="group" aria-label="card layout">
        <button
          :class="{ 'is-active': layout === 'columns' }"
          title="miller columns (synced to the URL)"
          @click="setLayout('columns')"
        >
          columns
        </button>
        <button
          :class="{ 'is-active': layout === 'tree' }"
          title="2D tree (in-memory only, not in the URL)"
          @click="setLayout('tree')"
        >
          tree
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.cards-root {
  display: flex;
  flex-direction: column;
  /* Fill whatever the shell's flex layout gives us (everything below
     the header); basis 0 + min-height 0 so intrinsic content height
     can't stretch the page. Negative margins bleed over the shell's
     1rem padding on the right and bottom so the status bar sits
     flush with the viewport bottom; the shell's left padding stays
     as a gutter. */
  flex: 1 1 0;
  min-height: 0;
  margin: 0 -1rem -1rem 0;
}
.cards-statusbar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 0.6rem;
  /* Bleed over the shell's left padding: the cards keep their gutter,
     but the status bar spans the full viewport width. */
  margin-left: -1rem;
  padding: 0.15rem 0.8rem;
  border-top: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
  font-size: 0.8rem;
  opacity: 0.85;
  min-height: 1.5rem;
}
.cards-statusbar code {
  font-family: ui-monospace, monospace;
  background: rgba(0, 0, 0, 0.12);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.cards-health--warn {
  color: #e35d6a;
}
.cards-statusbar-spacer {
  flex: 1 1 auto;
}
.cards-layout-toggle {
  display: flex;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  overflow: hidden;
}
.cards-layout-toggle button {
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 0.75rem;
  padding: 0.1rem 0.5rem;
}
.cards-layout-toggle button + button {
  border-left: 1px solid var(--fw-border);
}
.cards-layout-toggle button:hover {
  background: var(--fw-hover);
}
.cards-layout-toggle button.is-active {
  background: var(--fw-accent);
  color: var(--fw-bg);
}
</style>
