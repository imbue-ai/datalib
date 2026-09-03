<script setup lang="ts">
// The blocking screen for a data root the app cannot open.
//
// Why this exists, and why it blocks: `config.toml` declares the
// `unified_index` applet, and that applet *is* the grid, the search and
// the document view. So a config the server cannot use leaves every
// view in the app answering `502 {"error":"no applet
// \"unified_index\""}` — technically accurate, and a mystery, because
// the symptom and the cause are a whole screen apart (#199, #209). The
// app used to carry on in that state and let each view discover its own
// failure. It now stops here and says the one true thing.
//
// **It blocks for exactly two states, and no others.** `app_ready` is
// false when the file is not a config at all, or when it loads without
// a usable `unified_index` applet. A config with a *broken step* is
// neither: it loads, the app works, and that step's diagnostic belongs
// on its row in the Pipeline table, not in front of everything. Getting
// that line right is the whole point of the graded loader — before it,
// one stray key put you here.
//
// This screen is also reachable at any moment, not just at startup. An
// agent or an editor can break the file while the app is running, so
// `App.vue` drives this off the `config_changed` event and both
// directions matter: it must appear when the file breaks and — the part
// that is easy to get wrong — disappear on its own when the file is
// fixed, with no reload.
import { computed, nextTick, ref, watch } from "vue";
import { checkConfig, saveConfig, type ConfigResponse, type Diagnostic } from "@/api";

const props = defineProps<{ config: ConfigResponse }>();

const editor = ref<HTMLTextAreaElement | null>(null);
const text = ref(props.config.text);
const busy = ref(false);
const saveError = ref<string | null>(null);
// Diagnostics for the text in the box, which drifts from the server's
// as soon as the user types. Seeded from the load so the screen has
// something to say before it has been edited.
const live = ref<Diagnostic[]>(props.config.diagnostics);

// The file changing on disk under an open editor is normal here — this
// screen exists partly for the case where an agent is fixing the config
// — so adopt the new text unless the user has started typing over it.
const dirty = ref(false);
watch(
  () => props.config.text,
  (next) => {
    if (!dirty.value) {
      text.value = next;
      live.value = props.config.diagnostics;
    }
  },
);

// The two states this screen covers. They differ in what to say, not in
// what to do about it, so they share everything below the heading.
const notAConfig = computed(() => !props.config.parsed_ok);

const worst = computed(() =>
  live.value.find((d) => d.severity === "fatal") ?? live.value[0] ?? null,
);

let checkAt = 0;
async function recheck() {
  dirty.value = true;
  const mine = ++checkAt;
  try {
    const r = await checkConfig(text.value);
    // Ignore an answer that a later keystroke has already outdated.
    if (mine === checkAt) live.value = r.diagnostics;
  } catch {
    // A failed lint is not worth a message of its own: Save will say
    // whatever is really wrong, and the stale list is still useful.
  }
}

function selectSpan(d: Diagnostic) {
  const el = editor.value;
  if (!el || !d.span) return;
  el.focus();
  el.setSelectionRange(d.span[0], d.span[1]);
  // Put the selection in view: scrollHeight/lineCount is a good enough
  // line height for a monospace box, and being a line or two off is not
  // worth measuring for.
  const line = text.value.slice(0, d.span[0]).split("\n").length - 1;
  const lineHeight = el.scrollHeight / Math.max(1, text.value.split("\n").length);
  el.scrollTop = Math.max(0, (line - 4) * lineHeight);
}

async function save() {
  busy.value = true;
  saveError.value = null;
  try {
    const r = await saveConfig(text.value);
    live.value = r.diagnostics;
    if (!r.ok) {
      saveError.value = r.error ?? "The config was rejected.";
      return;
    }
    // Saved clean. Nothing else to do: the write raises
    // `config_changed`, `App.vue` refetches, `app_ready` comes back
    // true and this screen goes away by itself.
    dirty.value = false;
    await nextTick();
  } catch (e) {
    saveError.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

function severityLabel(d: Diagnostic): string {
  switch (d.severity) {
    case "fatal":
      return "not a config";
    case "rejected":
      return "dropped";
    case "blocked":
      return "can't run";
    case "warning":
      return "warning";
  }
}
</script>

<template>
  <section class="cfg-error">
    <div class="card">
      <template v-if="notAConfig">
        <h2>This config file can’t be read</h2>
        <p>
          <code class="root">{{ config.path }}</code> is not valid TOML, so
          nothing in it could be loaded — not your data sources, and not the
          <code>unified_index</code> applet that serves the table, search and
          document views. That is why the rest of the app is hidden: it has
          nothing to show until this file parses.
        </p>
      </template>
      <template v-else>
        <h2>This config declares no Unified Index</h2>
        <p>
          <code class="root">{{ config.path }}</code> loaded, but it has no
          usable <code>unified_index</code> applet. That applet is what serves
          the table, search and the document view, so every screen in the app
          would answer <code>no applet "unified_index"</code> without it.
        </p>
      </template>

      <p v-if="worst" class="lead" role="alert">
        <strong>{{ worst.message }}</strong>
        <span v-if="worst.line" class="where">— line {{ worst.line }}</span>
      </p>

      <p class="cli">
        From a terminal, the same check is
        <code>datalib-dag --check {{ config.path }}</code>.
      </p>

      <ul v-if="live.length" class="diags">
        <li v-for="(d, i) in live" :key="i" :class="['diag', `sev-${d.severity}`]">
          <button
            v-if="d.span"
            class="loc"
            title="select this in the editor below"
            @click="selectSpan(d)"
          >
            line {{ d.line }}
          </button>
          <span v-else class="loc loc-none">—</span>
          <span class="sev">{{ severityLabel(d) }}</span>
          <span class="body">
            <span v-if="d.entry" class="entry">{{ d.entry.kind }}
              {{ d.entry.id ? `"${d.entry.id}"` : `#${d.entry.index}` }}:</span>
            {{ d.message }}
            <span v-if="d.help" class="help">{{ d.help }}</span>
          </span>
        </li>
      </ul>
      <p v-else class="ok">No problems in the text below.</p>

      <label class="label" for="cfg-editor">Edit it here, or in your editor:</label>
      <textarea
        id="cfg-editor"
        ref="editor"
        v-model="text"
        class="editor"
        spellcheck="false"
        @input="recheck"
      />

      <div class="actions">
        <button class="primary" :disabled="busy" @click="save">
          {{ busy ? "Saving…" : "Save config" }}
        </button>
        <span v-if="saveError" class="error" role="alert">{{ saveError }}</span>
      </div>
    </div>
  </section>
</template>

<style scoped>
.cfg-error {
  flex: 1;
  display: flex;
  justify-content: center;
  align-items: flex-start;
  padding: 2rem 1rem;
}
.card {
  width: 100%;
  max-width: 56rem;
  border: 1px solid var(--datalib-border);
  border-radius: 6px;
  background: var(--datalib-card-bg);
  padding: 1.5rem 1.75rem;
}
h2 {
  margin: 0 0 0.75rem;
  font-size: 1.25rem;
}
p {
  margin: 0.6rem 0;
  line-height: 1.5;
}
code {
  background: var(--datalib-code-bg);
  border-radius: 3px;
  padding: 0.05rem 0.3rem;
  font-size: 0.9em;
}
.root {
  display: inline-block;
  overflow-wrap: anywhere;
}
.lead {
  color: var(--datalib-log-error);
}
.where {
  color: var(--datalib-muted);
  margin-left: 0.4rem;
}
.cli {
  color: var(--datalib-muted);
  font-size: 0.9rem;
}
.diags {
  list-style: none;
  margin: 0.8rem 0;
  padding: 0;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  overflow: hidden;
}
.diag {
  display: grid;
  grid-template-columns: 5.5rem 5.5rem 1fr;
  gap: 0.5rem;
  padding: 0.45rem 0.6rem;
  border-bottom: 1px solid var(--datalib-border);
  font-size: 0.9rem;
  line-height: 1.45;
}
.diag:last-child {
  border-bottom: none;
}
.loc {
  font: inherit;
  font-family: ui-monospace, monospace;
  background: none;
  border: none;
  padding: 0;
  color: var(--datalib-accent);
  cursor: pointer;
  text-align: right;
}
.loc-none {
  color: var(--datalib-muted);
  cursor: default;
}
.sev {
  font-variant: small-caps;
  letter-spacing: 0.02em;
}
.sev-fatal .sev,
.sev-rejected .sev {
  color: var(--datalib-log-error);
}
.sev-blocked .sev {
  color: var(--datalib-log-warn);
}
.sev-warning .sev {
  color: var(--datalib-muted);
}
.entry {
  font-family: ui-monospace, monospace;
  color: var(--datalib-muted);
}
.help {
  display: block;
  color: var(--datalib-muted);
}
.ok {
  color: var(--datalib-log-ok);
}
.label {
  display: block;
  margin-top: 0.9rem;
  color: var(--datalib-muted);
  font-size: 0.9rem;
}
.editor {
  width: 100%;
  box-sizing: border-box;
  min-height: 18rem;
  margin-top: 0.35rem;
  font-family: ui-monospace, monospace;
  font-size: 0.85rem;
  line-height: 1.5;
  tab-size: 2;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  padding: 0.6rem;
  resize: vertical;
}
.actions {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  margin-top: 0.75rem;
}
.error {
  color: var(--datalib-log-error);
}
button {
  font: inherit;
  padding: 0.45rem 0.9rem;
  border-radius: 4px;
  border: 1px solid var(--datalib-border);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  cursor: pointer;
}
button:disabled {
  cursor: default;
  opacity: 0.6;
}
button.primary {
  border-color: var(--datalib-accent);
  color: var(--datalib-accent);
  font-weight: 600;
}
</style>
