<script setup lang="ts">
// What "Free up disk space…" asks: collecting loses nothing, dropping old
// versions loses the history of what can be rebuilt. Dropping old versions
// collects too (`step_protocol.md` § Compact and collect), so ticking it
// ticks the first and holds it.
import { onMounted, ref, watch } from "vue";

const emit = defineEmits<{
  (e: "answer", answer: { ok: boolean; dropOldVersions: boolean }): void;
}>();

const collect = ref(true);
const dropOldVersions = ref(false);
watch(dropOldVersions, (drop) => {
  if (drop) collect.value = true;
});
const confirmButton = ref<HTMLButtonElement | null>(null);

onMounted(() => confirmButton.value?.focus());

function answer(ok: boolean) {
  emit("answer", { ok, dropOldVersions: dropOldVersions.value });
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") answer(false);
}
</script>

<template>
  <div class="dialog-backdrop" @click.self="answer(false)" @keydown="onKeydown">
    <div class="free dialog" role="dialog" aria-modal="true" aria-label="Free up disk space">
      <header class="dialog-head">
        <h2>Free up disk space</h2>
      </header>
      <div class="free-body dialog-body">
        <label class="free-check">
          <input
            v-model="collect"
            type="checkbox"
            :disabled="dropOldVersions"
            data-testid="free-collect"
          />
          <span>
            <strong>Delete unused data</strong>
            <span class="free-tag">loses nothing</span>
            <span class="free-help">
              Every sync leaves data behind that no version of your library uses any more. This
              deletes it from every store. Every version of every download and document stays
              readable.
            </span>
          </span>
        </label>
        <label class="free-check">
          <input v-model="dropOldVersions" type="checkbox" data-testid="free-drop-old" />
          <span>
            <strong>Drop old versions of rendered documents and the search index</strong>
            <span class="free-tag free-tag-lossy">loses history</span>
            <span class="free-help">
              They keep only their latest version. Both are rebuilt from your downloads, so no
              document is lost, but a source's History no longer shows earlier renders. The history
              of your downloads is kept.
            </span>
          </span>
        </label>
        <p class="free-note">
          Syncs that use these stores wait until it is done. On a large library that can take
          several minutes, and it needs about the size of the largest store free on disk while it
          runs.
        </p>
      </div>
      <footer class="dialog-foot">
        <button class="btn ghost" @click="answer(false)">Cancel</button>
        <button ref="confirmButton" class="btn primary" :disabled="!collect" @click="answer(true)">
          Free up space
        </button>
      </footer>
    </div>
  </div>
</template>

<style scoped src="./dialog.css"></style>
<style scoped>
.free {
  width: min(560px, 100%);
}
.free-body {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.free-check {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  cursor: pointer;
}
.free-check input {
  margin-top: 3px;
}
.free-tag {
  margin-left: 6px;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-muted);
}
.free-tag-lossy {
  color: var(--datalib-warn-fg);
}
.free-help {
  display: block;
  margin-top: 2px;
  color: var(--datalib-muted);
}
.free-note {
  margin: 0;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-muted);
}
</style>
