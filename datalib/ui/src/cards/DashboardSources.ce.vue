<script setup lang="ts">
// Each source's state: when it last synced, how many items it holds and
// how much space. A library with none says so, with a way to add one.
import { formatBytes } from "@/config/bytes";
import { uriFromEntity } from "./chipLinks";
import EntityChip from "./EntityChip.ce.vue";
import type { Dashboard } from "./useDashboard";

defineProps<{ d: Dashboard }>();
</script>

<template>
  <p v-if="d.loadError" class="dashboard-error">Could not load the sources: {{ d.loadError }}</p>
  <section class="panel" :class="{ 'panel-next': d.stage === 'empty' }" aria-label="Sources">
    <h2 class="panel-head">
      <span class="section-title">Sources</span>
      <button class="link" @click="d.open('sourcesView()')">Open Sources</button>
    </h2>
    <div v-if="d.sources.length" class="grid grid-head">
      <span>Name</span><span>Status</span><span>Updated</span><span class="num">Items</span
      ><span class="num">Size</span>
    </div>
    <div v-if="d.stage === 'empty'" class="row add-first">
      <span class="row-text">No sources yet.</span>
      <button class="dashboard-btn dashboard-btn-strong" @click="d.addSource">Add source</button>
    </div>
    <div v-for="r in d.sources" :key="r.id" class="grid grid-row">
      <span class="name">
        <EntityChip
          :uri="uriFromEntity('group', r.id)"
          :shown="r.name.label"
          quiet
          :open-card="(source) => d.open(source)"
        />
      </span>
      <span class="status" :class="d.statusClass(r)" :title="r.status.detail ?? ''"
        ><span class="dot" />{{ r.status.label }}</span
      >
      <span class="muted">{{ d.when(r.status.at) }}</span>
      <span class="num">{{ r.items.value?.toLocaleString() ?? "" }}</span>
      <span class="num muted">{{ r.disk.value != null ? formatBytes(r.disk.value) : "" }}</span>
    </div>
  </section>
</template>
