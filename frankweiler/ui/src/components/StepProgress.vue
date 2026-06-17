<script setup lang="ts">
// Discrete, honest progress: a segmented bar driven by the worker's
// `step x/n: Name` message (see src/sync/progress.ts). When the stage
// isn't known yet (e.g. "syncing…") or the job is in a terminal state, we
// fall back to a single bar rather than inventing segments.
import { computed } from "vue";
import { parseStep } from "@/sync/progress";

const props = defineProps<{
  msg: string | null;
  state: string;
}>();

const info = computed(() => parseStep(props.msg));

function segClass(i: number): string {
  const f = info.value;
  if (!f) return "";
  if (props.state === "done") return "done";
  if (i < f.step) return "done";
  if (i === f.step) {
    if (props.state === "failed") return "failed";
    if (props.state === "canceled") return "canceled";
    return "active";
  }
  return "";
}

const labelText = computed(() => {
  const f = info.value;
  if (f) {
    return `step ${f.step}/${f.total}: ${f.name}${f.detail ? ` (${f.detail})` : ""}`;
  }
  switch (props.state) {
    case "done":
      return "done";
    case "failed":
      return "failed";
    case "canceled":
      return "canceled";
    case "pending":
      return props.msg || "queued";
    default:
      return props.msg || "";
  }
});
</script>

<template>
  <div class="step-progress">
    <!-- Segmented bar once we know the stages. -->
    <div v-if="info" class="segs">
      <span
        v-for="i in info.total"
        :key="i"
        class="seg"
        :class="segClass(i)"
      />
    </div>
    <!-- Otherwise a single bar: full+green at done, red at failed, a
         pulsing sliver while pending/running with no stage yet. -->
    <div v-else class="single" :class="state">
      <span class="single-fill" />
    </div>
    <div class="step-label" :title="labelText">{{ labelText }}</div>
  </div>
</template>

<style scoped>
.step-progress {
  display: flex;
  flex-direction: column;
  gap: 0.2rem;
  min-width: 11rem;
}
.segs {
  display: flex;
  gap: 2px;
  height: 6px;
}
.seg {
  flex: 1;
  border-radius: 2px;
  background: var(--fw-border);
}
.seg.done {
  background: var(--fw-accent);
}
.seg.active {
  background: var(--fw-accent);
  animation: seg-pulse 1s ease-in-out infinite;
}
.seg.failed {
  background: #c0392b;
}
.seg.canceled {
  background: var(--fw-muted);
}
@keyframes seg-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.4;
  }
}
.single {
  position: relative;
  height: 6px;
  border-radius: 3px;
  background: var(--fw-border);
  overflow: hidden;
}
.single-fill {
  position: absolute;
  top: 0;
  left: 0;
  height: 100%;
  border-radius: 3px;
  background: var(--fw-accent);
  width: 100%;
}
.single.done .single-fill {
  background: #2e8b57;
}
.single.failed .single-fill {
  background: #c0392b;
}
.single.canceled .single-fill {
  background: var(--fw-muted);
}
/* pending/running with no recognized stage yet: sliding sliver */
.single.pending .single-fill,
.single.running .single-fill {
  width: 35%;
  animation: single-slide 1.1s ease-in-out infinite;
}
@keyframes single-slide {
  0% {
    left: -35%;
  }
  100% {
    left: 100%;
  }
}
.step-label {
  font-size: 0.74rem;
  color: var(--fw-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
