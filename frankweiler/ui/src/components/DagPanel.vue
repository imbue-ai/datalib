<script setup lang="ts">
// Inline host for the sourceDagView card outside the card system: the
// Sources page shows the live pipeline DAG while a sync runs. The view
// is a plain-DOM CardRender, so hosting it takes a shadow root and a
// stub CardCtx (no host commands / bus / title chrome apply here).
import { onMounted, onUnmounted, ref } from "vue";
import { sourceDagView } from "@/cards/libs/sourceDagView";
import type { CardCtx, Teardown } from "@/cards/types";

const hostEl = ref<HTMLDivElement | null>(null);
let teardown: Teardown | null = null;

onMounted(() => {
  const el = hostEl.value;
  if (!el) return;
  const root = el.attachShadow({ mode: "open" });
  const ctx: CardCtx = {
    cardId: "sources-dag-panel",
    initialState: "",
    setTitle: () => {},
    bus: { publish: () => {}, subscribe: () => () => {} },
    host: {
      openCards: () => [],
      setSource: () => {},
      close: () => {},
      setState: () => {},
    },
  };
  teardown = sourceDagView()(root, ctx);
});

onUnmounted(() => {
  teardown?.();
  teardown = null;
});
</script>

<template>
  <div ref="hostEl" class="dag-panel" />
</template>

<style scoped>
.dag-panel {
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  background: var(--fw-card-bg);
  overflow: hidden;
}
</style>
