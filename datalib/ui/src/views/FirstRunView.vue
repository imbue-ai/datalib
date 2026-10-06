<script setup lang="ts">
// First-run onboarding for a data root with no `config.toml`.
import { ref } from "vue";
import { useRouter } from "vue-router";
import { initConfig, type ConfigResponse } from "@/api";
import { MANAGE_STACK } from "@/router";

const props = defineProps<{ config: ConfigResponse }>();
const emit = defineEmits<{ (e: "initialized"): void }>();

const router = useRouter();

const busy = ref(false);
const error = ref<string | null>(null);

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
    emit("initialized");
    void router.replace(MANAGE_STACK);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <section class="first-run notice">
    <div class="card">
      <h2>Set up a data library</h2>
      <p>
        This folder is empty — there is no data library in it yet:
        <code class="root">{{ config.path }}</code>
      </p>
      <p>Initializing writes that one config file, and nothing else. It:</p>
      <ul>
        <li>
          declares the two index steps every source feeds — the grid index and the semantic vector
          index
        </li>
        <li>
          declares the <code>Unified Index</code> applet, which is what actually serves the table,
          search and document views
        </li>
        <li>
          adds <strong>no data sources</strong>: nothing is downloaded, no account is contacted, and
          nothing outside this folder is touched.
        </li>
      </ul>
      <p>
        Then you pick your first data source — a Slack export, a Claude export, a folder of PDFs —
        on the Manage screen this opens next.
      </p>
      <p v-if="error" class="error" role="alert">{{ error }}</p>
      <button class="primary" :disabled="busy" @click="initialize">
        {{ busy ? "Initializing…" : "Initialize empty data library" }}
      </button>
    </div>
  </section>
</template>

<style scoped src="./notice.css"></style>
<style scoped>
.card {
  max-width: 42rem;
}
ul {
  margin: 0.4rem 0 1rem;
  padding-left: 1.2rem;
  line-height: 1.5;
}
li {
  margin: 0.3rem 0;
}
.cmd {
  background: var(--datalib-code-bg);
  border-radius: var(--datalib-radius);
  font-family: var(--datalib-mono);
  padding: 0.6rem 0.75rem;
  overflow-x: auto;
  margin: 0;
}
.label {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
</style>
