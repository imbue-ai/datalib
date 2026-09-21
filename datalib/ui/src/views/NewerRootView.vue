<script setup lang="ts">
// The blocking screen for a data root a newer datalib wrote. Unlike the
// config screen there is nothing to edit here: the fix is to run the
// newer datalib, or to point this one at another root. The list is
// what the server refused to open, so the person can tell a stray
// store from a whole root.
import type { ConfigResponse, NewerRoot } from "@/api";

const props = defineProps<{ config: ConfigResponse }>();
const newer = (): NewerRoot => props.config.newer_root!;
// The newest line that wrote anything here: the version to run.
const needed = () =>
  newer()
    .stores.map((s) => s.wrote)
    .sort((a, b) => a.localeCompare(b, undefined, { numeric: true }))
    .at(-1);
</script>

<template>
  <section class="newer-root">
    <div class="card">
      <h2>This data root was written by a newer datalib</h2>
      <p>
        You are running datalib <code>{{ newer().running }}</code
        >, and stores under
        <code class="root">{{ config.path.replace(/\/config\.toml$/, "") }}</code> were last written
        by datalib <code>{{ needed() }}</code
        >. An older build can’t open a store a newer one wrote without losing what the newer one
        knew, so nothing here has been touched.
      </p>
      <p class="lead" role="alert">
        <strong
          >Run datalib {{ needed() }} or later against this root, or point this datalib at a
          different data root.</strong
        >
      </p>
      <ul class="stores">
        <li v-for="s in newer().stores" :key="s.store">
          <code>{{ s.store }}</code>
          <span class="wrote">written by {{ s.wrote }}</span>
        </li>
      </ul>
      <p class="cli">
        From a terminal, <code>datalib-dag --check {{ config.path }}</code>
        says the same.
      </p>
    </div>
  </section>
</template>

<style scoped>
.newer-root {
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
.stores {
  list-style: none;
  margin: 0.75rem 0;
  padding: 0;
}
.stores li {
  padding: 0.2rem 0;
}
.wrote {
  color: var(--datalib-muted);
  margin-left: 0.5rem;
  font-size: 0.9rem;
}
.cli {
  color: var(--datalib-muted);
  font-size: 0.9rem;
}
</style>
