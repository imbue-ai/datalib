<script setup lang="ts">
// Asked once a launch's migrate pass is over: the steps listed answered
// that what they wrote is in a shape this build cannot reach in place, and
// re-running them reads what is already downloaded. A step that could not
// be migrated is the toolbar's upgrade pill's to report, not this.
import { computed, onMounted, ref } from "vue";
import type { Upgrade } from "@/api";
import { rerenderSources } from "@/upgrade";

const props = defineProps<{ upgrade: Upgrade }>();

const emit = defineEmits<{
  (e: "answer", rerender: boolean): void;
}>();

const sources = computed(() => rerenderSources(props.upgrade));
const rerenderButton = ref<HTMLButtonElement | null>(null);

onMounted(() => rerenderButton.value?.focus());

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") emit("answer", false);
}
</script>

<template>
  <div class="dialog-backdrop" @keydown="onKeydown">
    <div
      class="rerender dialog"
      role="dialog"
      aria-modal="true"
      aria-label="Your rendered documents are out of date"
    >
      <header class="dialog-head">
        <h2>Your rendered documents are out of date</h2>
      </header>
      <div class="rerender-body dialog-body">
        <p>
          This version of datalib lays out rendered documents differently. Until they are rendered
          again, these sources show what the older version wrote, and a sync re-renders only the
          source it syncs:
        </p>
        <ul class="sources">
          <li v-for="s in sources" :key="s">{{ s }}</li>
        </ul>
        <p>Re-rendering reads what is already downloaded. Nothing is downloaded.</p>
      </div>
      <footer class="dialog-foot">
        <button class="btn ghost" @click="emit('answer', false)">Not now</button>
        <button ref="rerenderButton" class="btn primary" @click="emit('answer', true)">
          Re-render now
        </button>
      </footer>
    </div>
  </div>
</template>

<style scoped src="./dialog.css"></style>
<style scoped>
.rerender {
  width: min(560px, 100%);
}
.rerender-body {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.rerender-body p {
  margin: 0;
}
.sources {
  margin: 0;
  padding-left: 1.25rem;
}
</style>
