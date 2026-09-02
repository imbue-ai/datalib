<script setup lang="ts">
// First-run onboarding for a data root with no `config.toml`.
//
// Why this exists: pointing the app at an empty folder used to land
// straight on the grid, which asks the `unified_index` applet for rows
// — and that applet is declared *in the config*, so with no config the
// first thing a new user saw was
// `502 {"error":"no applet \"unified_index\""}`. Technically accurate,
// useless as a welcome.
//
// The rule this screen follows: say what is about to happen to the
// user's folder *before* touching it. It writes nothing on mount; the
// button does, and the copy above the button names the file and lists
// what goes in it.
//
// `App.vue` renders this instead of the router view while
// `/api/config` reports `exists: false`, so it covers every front door
// (browser, Tauri window) with one implementation.
//
// There is no "done" screen: initializing lands the user straight on
// the Manage view, because a library with no sources is not finished
// and adding one is the only useful next move.
import { ref } from "vue";
import { useRouter } from "vue-router";
import { initConfig, type ConfigResponse } from "@/api";

const props = defineProps<{ config: ConfigResponse }>();
const emit = defineEmits<{ (e: "initialized"): void }>();

const router = useRouter();

const busy = ref(false);
const error = ref<string | null>(null);

// A pre-TOML config.yaml in this root is not a fresh install — it's a
// migration waiting to happen, and writing an empty config.toml beside
// it would strand the sources it declares. The backend refuses too;
// this just doesn't offer the button.
const legacy = props.config.legacy_yaml_path;

async function initialize() {
  busy.value = true;
  error.value = null;
  try {
    const r = await initConfig();
    if (r.error) {
      error.value = r.error;
      return;
    }
    // `created: false` with no error means a config appeared while the
    // screen was open (a second window, an agent). Nothing went wrong
    // — the library is initialized, which is all this screen wanted.
    //
    // Straight to the Manage view: `initialized` drops the gate so
    // `App.vue` renders the router again, and the route is the one
    // with the "Add Data Source" button on it.
    emit("initialized");
    void router.replace("/sources2");
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <section class="first-run">
    <div class="card">
      <template v-if="legacy">
        <h2>This folder holds an older config</h2>
        <p>
          <code>{{ legacy }}</code> is a pre-TOML config. Datalib reads
          <code>config.toml</code> now, so this folder has to be converted
          before the app can open it — starting an empty library here would
          leave the sources in that file unread.
        </p>
        <p class="label">Run this once, then reopen the app:</p>
        <pre class="cmd">{{ config.legacy_migrate_cmd }}</pre>
      </template>

      <template v-else>
        <h2>Set up a data library</h2>
        <p>
          This folder is empty — there is no data library in it yet:
          <code class="root">{{ config.path }}</code>
        </p>
        <p>Initializing writes that one config file, and nothing else. It:</p>
        <ul>
          <li>
            declares the two index steps every source feeds — the grid index
            and the semantic vector index
          </li>
          <li>
            declares the <code>Unified Index</code> applet, which is what
            actually serves the table, search and document views
          </li>
          <li>
            adds <strong>no data sources</strong>: nothing is downloaded, no
            account is contacted, and nothing outside this folder is touched.
          </li>
        </ul>
        <p>
          Then you pick your first data source — a Slack export, a Claude
          export, a folder of PDFs — on the Manage screen this opens next.
        </p>
        <p v-if="error" class="error" role="alert">{{ error }}</p>
        <button class="primary" :disabled="busy" @click="initialize">
          {{ busy ? "Initializing…" : "Initialize empty data library" }}
        </button>
      </template>
    </div>
  </section>
</template>

<style scoped>
.first-run {
  flex: 1;
  display: flex;
  justify-content: center;
  align-items: flex-start;
  padding: 2rem 1rem;
}
.card {
  max-width: 42rem;
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
ul {
  margin: 0.4rem 0 1rem;
  padding-left: 1.2rem;
  line-height: 1.5;
}
li {
  margin: 0.3rem 0;
}
code {
  background: var(--datalib-code-bg);
  border-radius: 3px;
  padding: 0.05rem 0.3rem;
  font-size: 0.9em;
}
/* The data root path can be long; let it wrap rather than widen the card. */
.root {
  display: inline-block;
  overflow-wrap: anywhere;
}
.cmd {
  background: var(--datalib-code-bg);
  border-radius: 4px;
  padding: 0.6rem 0.75rem;
  overflow-x: auto;
  margin: 0;
}
.label {
  color: var(--datalib-muted);
  font-size: 0.9rem;
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
