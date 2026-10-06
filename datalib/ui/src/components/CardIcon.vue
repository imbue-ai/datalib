<script setup lang="ts">
// The icon beside a card's name in every layout: whatever the card's
// metadata names (cards/catalog.ts), so a builtin and a custom
// component are drawn the same way.
import { computed } from "vue";
import { cardMeta } from "@/cards/catalog";
import { ensureFrontend } from "@/cards/frontendRegistry";
import { resolveIcon } from "@/cards/icons";

const props = defineProps<{ source: string }>();

void ensureFrontend();
const icon = computed(() => resolveIcon(cardMeta(props.source)?.icon));
</script>

<template>
  <img v-if="icon.kind === 'image'" class="card-icon" :src="icon.url" alt="" />
  <svg v-else class="card-icon" viewBox="0 0 24 24" aria-hidden="true">
    <path fill="currentColor" :d="icon.path" />
  </svg>
</template>

<style scoped>
.card-icon {
  flex: 0 0 auto;
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
}
</style>
