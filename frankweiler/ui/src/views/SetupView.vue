<script setup lang="ts">
import { ref, onMounted, computed } from "vue";
import { useRouter } from "vue-router";
import {
  fetchConfig,
  fetchConfigScaffold,
  saveConfig,
  type ConfigResponse,
} from "@/api";

const router = useRouter();

const yaml = ref("");
const configPath = ref("");
const existed = ref(false);
const loadError = ref<string | null>(null);
// Parse/validation status of what's currently *saved* on disk.
const diskStatus = ref<{ ok: boolean; error: string | null; count: number } | null>(
  null,
);
// Result of the last Save attempt (null = unsaved edits or never saved).
const saveStatus = ref<{ ok: boolean; error: string | null; count: number } | null>(
  null,
);
const saving = ref(false);
const dirty = ref(false);

// Quick-add source stanzas. Each is a self-contained YAML list item the
// user can drop into `sources:`. Credentials are never here — they come
// from latchkey at runtime.
const SNIPPETS: { label: string; body: string }[] = [
  {
    label: "Claude",
    body: `  - name: claude
    type: claude_api
    sync: {}`,
  },
  {
    label: "ChatGPT",
    body: `  - name: chatgpt
    type: chatgpt_api
    sync: {}`,
  },
  {
    label: "Slack",
    body: `  - name: slack
    type: slack_api
    sync:
      media: true
      channels: ["general"]`,
  },
  {
    label: "GitHub",
    body: `  - name: github
    type: github_api
    sync: {}`,
  },
  {
    label: "GitLab",
    body: `  - name: gitlab
    type: gitlab_api
    sync: {}`,
  },
  {
    label: "Email (JMAP)",
    body: `  - name: fastmail
    type: email
    sync:
      hostname: api.fastmail.com`,
  },
  {
    label: "Contacts (vCard)",
    body: `  - name: contacts
    type: carddav
    input_path: ~/Downloads/contacts.vcf`,
  },
];

function onEdit() {
  dirty.value = true;
  saveStatus.value = null;
}

// Append a source stanza. If the YAML still has the scaffold's empty
// `sources: []`, flip it to a `sources:` block first so the appended
// item is valid.
function addSnippet(body: string) {
  let text = yaml.value;
  if (/^sources:\s*\[\s*\]\s*$/m.test(text)) {
    text = text.replace(/^sources:\s*\[\s*\]\s*$/m, "sources:");
  } else if (!/^sources:/m.test(text)) {
    text = text.replace(/\s*$/, "") + "\n\nsources:";
  }
  yaml.value = text.replace(/\s*$/, "") + "\n" + body + "\n";
  onEdit();
}

async function load() {
  loadError.value = null;
  try {
    const cfg: ConfigResponse = await fetchConfig();
    configPath.value = cfg.path;
    existed.value = cfg.exists;
    if (cfg.exists) {
      yaml.value = cfg.yaml;
      diskStatus.value = {
        ok: cfg.parsed_ok,
        error: cfg.error,
        count: cfg.source_count,
      };
    } else {
      // Fresh root — drop the scaffold into the editor so the user has a
      // valid starting point with `data_root` already filled in.
      const scaffold = await fetchConfigScaffold();
      yaml.value = scaffold.yaml;
      configPath.value = scaffold.path;
      diskStatus.value = null;
    }
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}

async function onSave() {
  saving.value = true;
  saveStatus.value = null;
  try {
    const res = await saveConfig(yaml.value);
    saveStatus.value = { ok: res.ok, error: res.error, count: res.source_count };
    if (res.ok) {
      dirty.value = false;
      existed.value = true;
      diskStatus.value = { ok: true, error: null, count: res.source_count };
    }
  } catch (e) {
    saveStatus.value = { ok: false, error: (e as Error).message, count: 0 };
  } finally {
    saving.value = false;
  }
}

const canGoToSync = computed(
  () => existed.value && !dirty.value && (diskStatus.value?.ok ?? false),
);

onMounted(load);
</script>

<template>
  <section class="setup-view">
    <div class="setup-header">
      <h2>Setup</h2>
      <div class="actions">
        <button class="btn" :disabled="saving" @click="load">Reload</button>
        <button class="btn btn-primary" :disabled="saving || !dirty" @click="onSave">
          {{ saving ? "Saving…" : "Save config" }}
        </button>
      </div>
    </div>

    <p class="intro">
      This data root keeps its own config. Edit it here, Save, then head to
      <RouterLink to="/sync">Sync</RouterLink> to pull your data in.
      Credentials aren't stored here — run
      <code>latchkey auth set &lt;provider&gt;</code> for each managed source.
    </p>

    <p v-if="loadError" class="status err">Could not load config: {{ loadError }}</p>

    <p class="path">
      <span class="label">File:</span> <code>{{ configPath }}</code>
      <span v-if="!existed" class="pill new">not created yet</span>
      <span v-else-if="diskStatus && !diskStatus.ok" class="pill bad">invalid</span>
      <span v-else-if="diskStatus" class="pill ok">{{ diskStatus.count }} source(s)</span>
    </p>

    <div class="snippets">
      <span class="label">Add a source:</span>
      <button
        v-for="s in SNIPPETS"
        :key="s.label"
        class="btn chip"
        @click="addSnippet(s.body)"
      >
        + {{ s.label }}
      </button>
    </div>

    <textarea
      v-model="yaml"
      class="editor"
      spellcheck="false"
      autocomplete="off"
      autocapitalize="off"
      @input="onEdit"
    />

    <div class="footer">
      <div class="save-status">
        <span v-if="saveStatus && saveStatus.ok" class="status ok">
          ✓ Saved — {{ saveStatus.count }} source(s) configured.
        </span>
        <span v-else-if="saveStatus && !saveStatus.ok" class="status err">
          ✗ Not saved: {{ saveStatus.error }}
        </span>
        <span v-else-if="dirty" class="status muted">unsaved changes</span>
      </div>
      <button
        class="btn btn-primary"
        :disabled="!canGoToSync"
        :title="canGoToSync ? '' : 'Save a valid config first'"
        @click="router.push('/sync')"
      >
        Go to Sync →
      </button>
    </div>
  </section>
</template>

<style scoped>
.setup-view {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  padding: 0 0.25rem;
  max-width: 60rem;
}
.setup-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.setup-header h2 {
  margin: 0;
  font-size: 1.1rem;
}
.actions {
  display: flex;
  gap: 0.5rem;
}
.intro {
  margin: 0;
  color: var(--fw-muted);
  font-size: 0.9rem;
  line-height: 1.5;
}
.path {
  margin: 0;
  font-size: 0.85rem;
}
.path .label,
.snippets .label {
  color: var(--fw-muted);
  margin-right: 0.4rem;
}
.snippets {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.4rem;
}
.editor {
  width: 100%;
  min-height: 24rem;
  box-sizing: border-box;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.82rem;
  line-height: 1.5;
  tab-size: 2;
  padding: 0.75rem;
  border: 1px solid var(--fw-border);
  border-radius: 6px;
  background: var(--fw-code-bg);
  color: var(--fw-fg);
  resize: vertical;
}
.footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
}
.btn {
  background: var(--fw-input-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  padding: 0.3rem 0.65rem;
  font-size: 0.85rem;
  cursor: pointer;
}
.btn:hover:not(:disabled) {
  background: var(--fw-hover);
}
.btn:disabled {
  opacity: 0.55;
  cursor: default;
}
.btn.chip {
  font-size: 0.78rem;
  padding: 0.2rem 0.5rem;
}
.btn-primary {
  background: var(--fw-accent);
  border-color: var(--fw-accent);
  color: white;
}
.btn-primary:hover:not(:disabled) {
  filter: brightness(1.08);
}
.pill {
  display: inline-block;
  margin-left: 0.5rem;
  font-size: 0.72rem;
  padding: 0.08rem 0.45rem;
  border-radius: 9999px;
  border: 1px solid var(--fw-border);
}
.pill.ok {
  color: #2e8b57;
  border-color: #2e8b57;
}
.pill.bad {
  color: #c0392b;
  border-color: #c0392b;
}
.pill.new {
  color: var(--fw-muted);
}
.status {
  font-size: 0.85rem;
}
.status.ok {
  color: #2e8b57;
}
.status.err {
  color: #c0392b;
}
.status.muted {
  color: var(--fw-muted);
}
code {
  background: var(--fw-code-bg);
  padding: 0 0.25rem;
  border-radius: 2px;
  font-size: 0.8rem;
}
</style>
