<script setup lang="ts">
// Mounts one card inside a Shadow DOM. The card is defined by its
// source (a JS expression like `documentView("abcd…")`): we compile
// it via cardSource.ts and run the resulting CardRender inside the
// shadow root. The render function gets full DOM ownership of the
// shadow root — Vue doesn't render anything inside. When the source
// changes we tear the old card down and run the new one; on unmount
// we call the teardown returned by the render.
import { onMounted, onBeforeUnmount, shallowRef, useTemplateRef, watch } from "vue";
import { compileCardSource } from "@/cards/cardSource";
import type { CardCtx, Teardown } from "@/cards/types";

const props = defineProps<{
  source: string;
  ctx: CardCtx;
}>();

const hostEl = useTemplateRef<HTMLDivElement>("hostEl");
const shadow = shallowRef<ShadowRoot | null>(null);
const teardown = shallowRef<Teardown | null>(null);

function tearDownCard() {
  const fn = teardown.value;
  if (!fn) return;
  teardown.value = null;
  try {
    fn();
  } catch (e) {
    console.error("[shadow card teardown]", e);
  }
}

function runCard() {
  const root = shadow.value;
  if (!root) return;
  tearDownCard();
  root.replaceChildren();
  if (props.source.trim() === "") {
    const div = document.createElement("div");
    div.style.cssText =
      "opacity:.45;padding:12px;font:12px ui-monospace,monospace";
    div.textContent = "empty card — type source above and press Enter";
    root.appendChild(div);
    return;
  }
  try {
    const render = compileCardSource(props.source);
    teardown.value = render(root, props.ctx);
  } catch (e) {
    const div = document.createElement("div");
    div.style.cssText =
      "color:#e35d6a;padding:8px;font-family:ui-monospace,monospace;font-size:12px;white-space:pre-wrap";
    div.textContent =
      "card error: " +
      ((e as Error).stack ?? (e as Error).message ?? String(e));
    root.appendChild(div);
  }
}

onMounted(() => {
  const el = hostEl.value;
  if (!el) return;
  shadow.value = el.attachShadow({ mode: "open" });
  runCard();
});

watch(
  () => props.source,
  () => runCard(),
);

onBeforeUnmount(tearDownCard);
</script>

<template>
  <div ref="hostEl" class="shadow-card-host" />
</template>

<style scoped>
/* Height comes from the parent (flex sizing on the host's card slot)
   — a height: 100% here would resolve against the whole column/node
   including its chrome bar and overflow by that much. */
.shadow-card-host {
  width: 100%;
  overflow: hidden;
  box-sizing: border-box;
}
</style>
