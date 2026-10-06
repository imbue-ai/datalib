<script setup lang="ts">
// What needs the person: each source whose last sync did not finish
// well, or whose store holds errors or warnings, with the fix beside it.
// Nothing at all when nothing does.
import { iconUrl } from "@/config/icons";
import type { Dashboard } from "./useDashboard";

defineProps<{ d: Dashboard }>();
</script>

<template>
  <section v-if="d.attention.length" class="panel panel-warn" aria-label="Needs you">
    <h2 class="panel-head panel-head-warn">
      <span class="section-title">Needs you · {{ d.attention.length }}</span>
    </h2>
    <div v-for="a in d.attention" :key="a.row.id" class="row">
      <img v-if="iconUrl(a.row.name.icon)" class="tile" :src="iconUrl(a.row.name.icon)!" alt="" />
      <span class="row-text">
        <strong>{{ a.row.name.label }}</strong> {{ a.text }}
      </span>
      <button v-if="a.problems" class="link" @click="d.openProblems(a.row)">Review</button>
      <template v-if="a.failed">
        <button class="link" @click="d.openLog(a.row)">View log</button>
        <button class="dashboard-btn dashboard-btn-strong" @click="d.syncRow(a.row)">
          Sync again
        </button>
      </template>
    </div>
  </section>
</template>
