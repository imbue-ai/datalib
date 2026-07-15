<script setup lang="ts">
import { ref, onMounted } from "vue";
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

// YYYY-MM-DD for `n` days before today (UTC).
function isoDaysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
}

// Quick-add source stanzas. Each is a self-contained YAML list item the
// user can drop into `sources:`. The orchestrator owns only `name`/
// `enabled`; everything provider-owned (including `type:`) nests under
// `source:` — see `frankweiler/backend/ingest_config`. Credentials are
// never here — they come from latchkey at runtime. Bodies are functions
// so date-dependent snippets (Slack's `since:`) are computed at click
// time.
// How to invoke the latchkey CLI in the snippets below. The backend
// reports the right form for this install (the app-bundled launcher's
// path, or an npx fallback) on ConfigResponse; until the first config
// fetch resolves we show the npx form, which works everywhere Node
// does.
const latchkeyCli = ref("npx -y latchkey");

const SNIPPETS: { label: string; body: () => string }[] = [
  {
    label: "Claude",
    body: () => `  # Prerequisite (one-time): register claude.ai with latchkey and
  # supply your sessionKey cookie (DevTools → Application → Cookies):
  #   ${latchkeyCli.value} services register claude-ai --base-api-url="https://claude.ai/"
  #   ${latchkeyCli.value} auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
  # See docs/user/getting_your_data.md for the full walkthrough.
  - name: claude
    source:
      type: claude_api
      sync: {}`,
  },
  {
    label: "ChatGPT",
    body: () => `  - name: chatgpt
    source:
      type: chatgpt_api
      sync: {}`,
  },
  {
    // `since:` starts the backfill 30 days back so the first sync stays
    // small; users widen it once they've seen a sync succeed.
    label: "Slack",
    body: () => `  - name: slack
    source:
      type: slack_api
      sync:
        media: true
        channels: ["general"]
        since: "${isoDaysAgo(30)}"`,
  },
  {
    label: "GitHub",
    body: () => `  - name: github
    source:
      type: github_api
      sync: {}`,
  },
  {
    label: "GitLab",
    body: () => `  - name: gitlab
    source:
      type: gitlab_api
      sync: {}`,
  },
  {
    label: "Email (JMAP)",
    body: () => `  - name: fastmail
    source:
      type: email
      sync:
        hostname: api.fastmail.com`,
  },
  {
    // `input_path` is part of the shared per-source envelope, so it
    // lives under `common:`, not at the top of `source:`.
    label: "Contacts (vCard)",
    body: () => `  - name: contacts
    source:
      type: carddav
      common:
        input_path: ~/Downloads/contacts.vcf`,
  },
  {
    // Sample public source — no latchkey needed. Bare `sync: {}` pulls
    // the default Thucydides Histories (Greek + English) from PerseusDL.
    label: "Perseus (sample)",
    body: () => `  - name: perseus
    source:
      type: perseus
      sync: {}`,
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
    if (cfg.latchkey_cli) latchkeyCli.value = cfg.latchkey_cli;
    if (cfg.exists) {
      yaml.value = cfg.yaml;
      diskStatus.value = {
        ok: cfg.parsed_ok,
        error: cfg.error,
        count: cfg.source_count,
      };
    } else {
      // Fresh root — drop the scaffold into the editor so the user has a
      // valid starting point to add sources to.
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

// Save (unless there's provably nothing to do — no unsaved edits and a
// valid config already on disk) and move on to Sync. Stays put when
// validation fails; the inline ✗ status explains why.
async function onSaveAndGo() {
  const savedAndValid = !dirty.value && (diskStatus.value?.ok ?? false);
  if (!savedAndValid) {
    await onSave();
    if (!saveStatus.value?.ok) return;
  }
  router.push("/sync");
}

// The tab reloads the config every time it's switched to: the router
// mounts a fresh SetupView per visit (no KeepAlive), so onMounted runs
// on every switch.
onMounted(load);
</script>

<template>
  <section class="setup-view">
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
        @click="addSnippet(s.body())"
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
      <div class="actions">
        <button class="btn" :disabled="saving || !dirty" @click="onSave">
          {{ saving ? "Saving…" : "Save" }}
        </button>
        <button class="btn btn-primary" :disabled="saving" @click="onSaveAndGo">
          Save and go to Sync →
        </button>
      </div>
    </div>
  </section>
</template>

<style scoped>
.setup-view {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  padding: 0 0.25rem;
}
.actions {
  display: flex;
  gap: 0.5rem;
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
  /* Re-assert the accent background: `.btn:hover:not(:disabled)` above
     has equal specificity and would otherwise paint the generic light
     hover background under this button's white text. */
  background: var(--fw-accent);
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
