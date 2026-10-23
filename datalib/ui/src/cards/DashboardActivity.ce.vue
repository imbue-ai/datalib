<script setup lang="ts">
// The newest documents in the library, each opening its document.
import { iconUrl } from "@/config/icons";
import EntityChip from "./EntityChip.ce.vue";
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
    <!-- A row, not a button: a source's chip sits inside it, and a link
         inside a button is not allowed. -->
    <div
      v-for="doc in d.recent"
      :key="doc.uuid"
      class="row recent"
      role="link"
      tabindex="0"
      @click="d.openDocument(doc)"
      @keydown.enter="d.openDocument(doc)"
    >
      <EntityChip
        v-if="doc.source_ref?.entity"
        :uri="doc.source_ref.entity"
        :shown="doc.source_ref.label"
        quiet
        :open-card="(source) => d.open(source)"
      />
      <img
        v-else-if="iconUrl(doc.source_ref?.icon)"
        class="tile tile-small"
        :src="iconUrl(doc.source_ref?.icon)!"
        alt=""
      />
      <strong class="recent-title">{{ doc.conversation_name || doc.kind }}</strong>
      <span class="recent-snippet">{{ doc.snippet }}</span>
      <span class="muted">{{ d.when(doc.touched_at) }}</span>
    </div>
  </section>
</template>
