<script setup lang="ts">
import { ref } from "vue";

const props = defineProps<{
  uuid: string;
  label?: string;
}>();

const state = ref<"idle" | "copied" | "failed">("idle");

async function onClick() {
  try {
    await navigator.clipboard.writeText(props.uuid);
    state.value = "copied";
  } catch {
    state.value = "failed";
  }
  setTimeout(() => (state.value = "idle"), 900);
}
</script>

<template>
  <button
    type="button"
    class="copy-uuid"
    :class="{ copied: state === 'copied', 'copy-failed': state === 'failed' }"
    :title="`${label || 'Copy ID'} (${uuid})`"
    :aria-label="label || 'Copy ID'"
    @click.stop.prevent="onClick"
  >
    {{ state === "copied" ? "✓" : "🆔" }}
  </button>
</template>
