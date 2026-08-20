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
  ensureFrontend,
  followRenames,
  frontendManifest,
  referencedNamespaces,
} from "@/cards/frontendRegistry";
import type { CardCtx, Teardown } from "@/cards/types";

const props = defineProps<{
  source: string;
  ctx: CardCtx;
}>();

const hostEl = useTemplateRef<HTMLDivElement>("hostEl");
const shadow = shallowRef<ShadowRoot | null>(null);
const teardown = shallowRef<Teardown | null>(null);

// The `comp.<ns>.<name>` references the current source makes, and the
// component hashes they resolved to at last compile. The manifest
// watcher recompiles when any of them appears, changes, or disappears —
// so a card re-renders the moment an agent re-saves a component it uses
// (even one that didn't exist yet at first compile).
let watchedNames = new Set<string>();
let watchedHashes = new Map<string, string | undefined>();

// Every `comp.<ns>.<name>` in a piece of source, as "ns.name". The
// namespace scan is over-approximate on purpose; pairing it with the
// name here keeps the watch precise.
function referencedComponents(source: string): Set<string> {
  const out = new Set<string>();
  const re = /\bcomp\s*\.\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*\.\s*([A-Za-z_$][A-Za-z0-9_$]*)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(source)) !== null) out.add(`${m[1]}.${m[2]}`);
  return out;
}

function componentHash(qualified: string): string | undefined {
  const [ns, name] = qualified.split(".");
  const meta = frontendManifest.value.get(ns)?.get(name);
  if (!meta) return undefined;
  return "renamed_to" in meta ? `renamed:${meta.renamed_to}` : meta.component_hash;
}

// Compilation is async (resolving aliases fetches their source); a
// newer run must win. Bumped on every runCard; stale runs bail before
// mutating the DOM.
let runToken = 0;

function snapshotWatched(source: string) {
  watchedNames = referencedComponents(source);
  watchedHashes = new Map();
  for (const name of watchedNames) {
    watchedHashes.set(name, componentHash(name));
  }
}

// A component this card references may have been renamed (an agent
// giving its placeholder a formal name — see the store's rename
// tombstones). Rewrite the source to the new name and repoint the card
// through the host; returns whether a rewrite happened (the setSource
// comes back as a source-prop change, which re-runs the card). A
// tombstone holds no component, so following it is the only way the
// card keeps working.
//
// Renames stay inside one namespace, which is what makes the rewrite a
// safe textual substitution: only the member after the namespace moves.
function applyRenames(): boolean {
  let src = props.source;
  for (const qualified of referencedComponents(props.source)) {
    const [ns, name] = qualified.split(".");
    const target = followRenames(ns, name);
    if (target && target !== name) {
      src = src.replace(
        new RegExp(`\\bcomp\\s*\\.\\s*${ns}\\s*\\.\\s*${name}\\b`, "g"),
        `comp.${ns}.${target}`,
      );
    }
  }
  if (src === props.source) return false;
  props.ctx.host.setSource(src);
  return true;
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
    await ensureFrontend();
    // A referenced alias may have been renamed while this card wasn't
    // mounted (an old URL, a layout toggle): rewrite before compiling
    // so the stale name never error-flashes. The setSource re-enters
    // runCard with the new source.
    if (applyRenames()) return;
    const { render } = await compileCardSource(props.source);
    // A newer run started while we were awaiting — drop this one.
    if (token !== runToken || shadow.value !== root) return;
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

// One watcher over the store manifest, handling both ways a card can
// go stale.
//
// A rename has to be checked first: a tombstone carries no component
// hash, so the recompile branch below would never fire for it, and the
// card would sit pointed at a name that no longer resolves. Rewriting
// the source repoints the card, which re-enters runCard through the
// source-prop watcher above.
watch(frontendManifest, () => {
  if (applyRenames()) return;
  // Re-render when a component this card references changes hash,
  // appears, or disappears.
  for (const name of watchedNames) {
    if (componentHash(name) !== watchedHashes.get(name)) {
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
