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
import { devMode } from "@/devMode";
import {
  aliasManifest,
  ensureManifest,
  referencedIdentifiers,
} from "@/cards/aliasRegistry";
import type { CardCtx, Teardown } from "@/cards/types";

const props = defineProps<{
  source: string;
  ctx: CardCtx;
}>();

const hostEl = useTemplateRef<HTMLDivElement>("hostEl");
const shadow = shallowRef<ShadowRoot | null>(null);
const teardown = shallowRef<Teardown | null>(null);

// Alias names the current source mentions, and the manifest hashes they
// had at last compile. The manifest watcher recompiles when any of
// these names appears, changes, or disappears — so a card pointed at an
// alias re-renders the moment an agent re-saves that alias (even if the
// alias didn't exist yet at first compile).
let watchedNames = new Set<string>();
let watchedHashes = new Map<string, string | undefined>();

// Compilation is async (resolving aliases fetches their source); a
// newer run must win. Bumped on every runCard; stale runs bail before
// mutating the DOM.
let runToken = 0;

function snapshotWatched(source: string) {
  watchedNames = referencedIdentifiers(source);
  watchedHashes = new Map();
  for (const name of watchedNames) {
    watchedHashes.set(name, aliasManifest.value.get(name));
  }
}

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

async function runCard() {
  const root = shadow.value;
  if (!root) return;
  const token = ++runToken;
  tearDownCard();
  root.replaceChildren();
  // Reset the title to the source-derived fallback; the card's render
  // (below) declares its own via ctx.setTitle, typically first thing.
  // Doing this on every run means a re-run never shows the previous
  // card's stale title, and blank/error runs need nothing special.
  props.ctx.setTitle(null);
  snapshotWatched(props.source);
  if (props.source.trim() === "") {
    // Sole onboarding text for an empty card — the source textarea
    // above stays blank (no placeholder), so the how-to lives here.
    // Blank cards are created in dev mode, but one can outlive a
    // toggle to non-dev (where the source box is gone) — track the
    // flag so the text never points at a textarea that isn't there.
    const div = document.createElement("div");
    div.style.cssText =
      "opacity:.45;padding:12px;font:12px ui-monospace,monospace;" +
      "display:flex;flex-direction:column;gap:6px";
    root.appendChild(div);
    const paintBlank = (dev: boolean) => {
      div.replaceChildren();
      const intro = document.createElement("div");
      div.appendChild(intro);
      if (!dev) {
        intro.textContent =
          "empty card — turn on dev mode to type source, or close it";
        return;
      }
      intro.textContent =
        "empty card — type source above and press Enter, e.g.:";
      const examples = [
        "gridView()",
        'documentView("uuid")',
        "galleryView()",
        "aliasView()",
        "dactalView()",
        '(root) => { root.textContent = "hello, world" }',
      ];
      for (const ex of examples) {
        const code = document.createElement("code");
        code.style.cssText = "margin-left:1em";
        code.textContent = ex;
        div.appendChild(code);
      }
    };
    const stop = watch(devMode, paintBlank, { immediate: true });
    teardown.value = () => stop();
    return;
  }
  try {
    await ensureManifest();
    const { render, deps } = await compileCardSource(props.source);
    // A newer run started while we were awaiting — drop this one.
    if (token !== runToken || shadow.value !== root) return;
    // Watch the resolved transitive closure (plus whatever the source
    // names) so a change to any dependency re-renders.
    for (const name of deps) {
      watchedNames.add(name);
      if (!watchedHashes.has(name)) {
        watchedHashes.set(name, aliasManifest.value.get(name));
      }
    }
    teardown.value = render(root, props.ctx);
  } catch (e) {
    if (token !== runToken || shadow.value !== root) return;
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
  void runCard();
});

watch(
  () => props.source,
  () => void runCard(),
);

// Re-render when an alias this card depends on (or references by name)
// changes hash, appears, or disappears in the library manifest.
watch(aliasManifest, (m) => {
  for (const name of watchedNames) {
    if (m.get(name) !== watchedHashes.get(name)) {
      void runCard();
      return;
    }
  }
});

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
