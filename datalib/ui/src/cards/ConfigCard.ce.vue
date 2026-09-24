<script setup lang="ts">
// `configView()`: `config.toml` itself, edited directly. The file is
// the source of truth; every other card is a view of it. This is where
// to go for anything the forms don't model — a source type with no
// wizard yet, or a knob that would make a row's Edit button refuse.
// A save goes through the backend's own loader, so a rejection comes
// back as the loader's message rather than a broken root; a reload
// never overwrites unsaved text.
import { onBeforeUnmount, onMounted, ref } from "vue";
import { useApi } from "@/cards/cardApi";
import { subscribeLive } from "@/live";
import { isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";
import { TOPIC_CONFIG_WRITTEN, type CardCtx } from "./types";

const { fetchConfig, fetchConfigScaffold, saveConfig } = useApi();

const props = defineProps<{ ctx: CardCtx }>();

const text = ref("");
const path = ref("");
const dirty = ref(false);
const busy = ref(false);
const cardEl = ref<HTMLElement | null>(null);
const banner = ref<{ ok: boolean; text: string } | null>(null);
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

props.ctx.setTitle("config.toml");
props.ctx.setHelp(`
<p>This is <code>config.toml</code>, the one file everything else is a view of:
every source, every step, every applet. Edit it here for anything the
Add/Edit forms don't model — a source type with no wizard yet, a knob a
form would refuse to keep.</p>
<p><b>Save</b> runs the file through the backend's own loader before it
lands: an entry the loader can't use is dropped with a reason, the rest
of the pipeline keeps running, and the Sources table shows the dropped
entry as <i>Not loaded</i> with why. A file that isn't TOML at all is
refused and nothing is written.</p>
<p>A change made elsewhere — another window, an agent, a text editor —
arrives here on its own, unless you have unsaved text, which stays until
you save or discard it.</p>
`);

async function load() {
  try {
    let cfg = await fetchConfig();
    if (!cfg.exists) cfg = await fetchConfigScaffold();
    path.value = cfg.path;
    // A reload must never overwrite what someone is typing. Their text
    // wins until they save or discard.
    if (dirty.value) return;
    text.value = cfg.text;
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  }
}

function onEdit() {
  dirty.value = true;
  banner.value = null;
}

async function save() {
  busy.value = true;
  banner.value = null;
  try {
    const res = await saveConfig(text.value);
    if (!res.ok) {
      banner.value = { ok: false, text: res.error ?? "The config was rejected." };
      return;
    }
    dirty.value = false;
    // A warning saves — nothing is dropped — but it is still advice the
    // file would otherwise only give on the command line.
    banner.value = {
      ok: true,
      text: res.error ? `Saved the config. Warning: ${res.error}` : "Saved the config.",
    };
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

async function discard() {
  dirty.value = false;
  banner.value = null;
  await load();
}

let unsubscribe: (() => void) | null = null;
let unsubscribeBus: (() => void) | null = null;
onMounted(() => {
  void load();
  unsubscribe = subscribeLive(
    {
      root: (e) => {
        if (e.kind === "config_changed") void load();
      },
      resync: () => void load(),
    },
    { onScreen: cardEl.value ?? undefined },
  );
  unsubscribeBus = props.ctx.bus.subscribe(TOPIC_CONFIG_WRITTEN, () => void load());
});
onBeforeUnmount(() => {
  unsubscribe?.();
  unsubscribeBus?.();
});
</script>

<template>
  <div ref="cardEl" class="cfg">
    <p class="cfg-file">
      <code>{{ path }}</code>
      <button
        v-if="canReveal"
        class="cfg-btn"
        :title="`${revealLabel} — the config file`"
        @click="revealInFileManager(path)"
      >
        {{ revealLabel }}
      </button>
    </p>
    <textarea v-model="text" class="m2-editor cfg-editor" spellcheck="false" @input="onEdit" />
    <div class="cfg-actions">
      <button class="cfg-btn" :disabled="!dirty || busy" @click="save">Save</button>
      <button class="cfg-btn muted" :disabled="!dirty || busy" @click="discard">
        Discard changes
      </button>
      <span v-if="dirty" class="cfg-dirty"
        >Unsaved — the Sources table still shows the last saved version.</span
      >
      <span v-if="banner" class="cfg-banner" :class="banner.ok ? 'good' : 'bad'">{{
        banner.text
      }}</span>
    </div>
  </div>
</template>

<style>
.cfg {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  padding: 10px 12px;
  gap: 8px;
  box-sizing: border-box;
}
.cfg-file {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  margin: 0;
  font-size: 12px;
  color: var(--datalib-muted);
}
.cfg-file code {
  word-break: break-all;
}
.cfg-editor {
  flex: 1 1 auto;
  min-height: 0;
  width: 100%;
  padding: 10px 12px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12.5px;
  line-height: 1.55;
  resize: none;
  box-sizing: border-box;
}
.cfg-actions {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
}
.cfg-dirty,
.cfg-banner {
  font-size: 12px;
  color: var(--datalib-muted);
}
.cfg-banner.good {
  color: var(--datalib-log-ok);
}
.cfg-banner.bad {
  color: var(--datalib-log-error);
}
.cfg-btn {
  padding: 2px 9px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-card-bg);
  color: inherit;
  font: inherit;
  font-size: 12px;
  cursor: pointer;
}
.cfg-btn:hover:not(:disabled) {
  background: var(--datalib-hover);
}
.cfg-btn:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
.cfg-btn.muted {
  color: var(--datalib-muted);
}
</style>
