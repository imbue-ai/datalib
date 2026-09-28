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
  <section class="newer-root notice">
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

<style scoped src="./notice.css"></style>
<style scoped>
.card {
  width: 100%;
  max-width: 56rem;
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
</style>
