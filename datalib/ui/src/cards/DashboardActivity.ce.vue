<script setup lang="ts">
// The newest documents in the library, each opening its document.
import { iconUrl } from "@/config/icons";
import type { Dashboard } from "./useDashboard";

defineProps<{ d: Dashboard }>();
</script>

<template>
  <section class="panel" aria-label="Latest activity">
    <h2 class="panel-head">
      <span class="section-title">Latest activity</span>
      <button class="link" @click="d.open('searchView()')">Search everything</button>
    </h2>
    <p v-if="d.recent.length === 0" class="empty">Nothing indexed yet.</p>
    <button v-for="doc in d.recent" :key="doc.uuid" class="row recent" @click="d.openDocument(doc)">
      <img
        v-if="iconUrl(doc.source_ref?.icon)"
        class="tile tile-small"
        :src="iconUrl(doc.source_ref?.icon)!"
        alt=""
      />
      <strong class="recent-title">{{ doc.conversation_name || doc.kind }}</strong>
      <span class="recent-snippet">{{ doc.snippet }}</span>
      <span class="muted">{{ d.when(doc.touched_at) }}</span>
    </button>
  </section>
</template>
