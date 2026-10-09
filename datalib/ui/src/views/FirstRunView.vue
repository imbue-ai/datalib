<script setup lang="ts">
// First-run onboarding for a data root with no `config.toml`.
import { computed, ref } from "vue";
import { useRouter } from "vue-router";
import { initConfig, type ConfigResponse } from "@/api";
import { MANAGE_STACK } from "@/router";

const props = defineProps<{ config: ConfigResponse }>();
const emit = defineEmits<{ (e: "initialized"): void }>();

const router = useRouter();
const folder = computed(() => props.config.path.replace(/[/\\][^/\\]*$/, ""));

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
      <h2>Initialize data library</h2>
      <p>
        There is no data library in this folder yet:
        <code class="root">{{ folder }}</code>
      </p>
      <p>
        Initializing the data library creates a bare-bones config file. You will be able to add data
        sources later.
      </p>
      <p v-if="error" class="error" role="alert">{{ error }}</p>
      <button class="primary" :disabled="busy" @click="initialize">
        {{ busy ? "Initializing…" : "Initialize data library" }}
      </button>
      <details>
        <summary>What this writes to the folder</summary>
        <p>
          One config file, and nothing else:
          <code class="root">{{ config.path }}</code>
        </p>
        <p>The file:</p>
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
            adds <strong>no data sources</strong>: nothing is downloaded, no account is contacted,
            and nothing outside this folder is touched.
          </li>
        </ul>
      </details>
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
details {
  margin-top: 1rem;
  color: var(--datalib-muted);
}
summary {
  cursor: pointer;
}
</style>
