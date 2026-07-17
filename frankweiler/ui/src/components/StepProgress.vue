<script setup lang="ts">
// Per-task progress: one cell per DAG task (see src/sync/progress.ts).
// Done tasks are green (succeeded / skipped-up-to-date) or red
// (failed; blocked shows muted — it never ran, its upstream failed),
// running tasks flash yellow, todo tasks are the remaining blank
// space. Multiple tasks run concurrently, so every running task's
// sub-detail is listed under the bar. When no board is known yet
// (queued, legacy rows) we fall back to a single indeterminate bar.
import { computed } from "vue";
import { parseTasks } from "@/sync/progress";

const props = defineProps<{
  msg: string | null;
  state: string;
}>();

const tasks = computed(() => parseTasks(props.msg));

function cellClass(state: string): string {
  switch (state) {
    case "done":
    case "skipped":
      return "ok";
    case "failed":
      return "failed";
    case "blocked":
      return "blocked";
    case "running":
      // A cancel leaves mid-flight tasks behind; stop the flashing.
      return props.state === "running" ? "running" : "stale";
    default:
      return ""; // todo → blank
  }
}

function cellTitle(t: {
  id: string;
  state: string;
  detail?: string | null;
}): string {
  return t.detail ? `${t.id}: ${t.state} — ${t.detail}` : `${t.id}: ${t.state}`;
}

const running = computed(() =>
  (tasks.value ?? []).filter((t) => t.state === "running"),
);

const labelText = computed(() => {
  const ts = tasks.value;
  if (ts) {
    const terminal = ts.filter((t) =>
      ["done", "skipped", "failed", "blocked"].includes(t.state),
    ).length;
    const failed = ts.filter((t) => t.state === "failed").length;
    const base = `${terminal}/${ts.length} tasks`;
    return failed > 0 ? `${base} (${failed} failed)` : base;
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
    <!-- One cell per task once the board is known. -->
    <div v-if="tasks" class="cells">
      <span
        v-for="t in tasks"
        :key="t.id"
        class="cell"
        :class="cellClass(t.state)"
        :title="cellTitle(t)"
      />
    </div>
    <!-- Otherwise a single bar: full+green at done, red at failed, a
         pulsing sliver while pending/running with no board yet. -->
    <div v-else class="single" :class="state">
      <span class="single-fill" />
    </div>
    <div class="step-label" :title="labelText">{{ labelText }}</div>
    <!-- All concurrently-running tasks, each with its own live detail. -->
    <div
      v-for="t in running"
      :key="t.id"
      class="task-line"
      :title="t.detail || ''"
    >
      <span class="task-id">{{ t.id }}</span>
      <span v-if="t.detail" class="task-detail">{{ t.detail }}</span>
    </div>
  </div>
</template>

<style scoped>
.step-progress {
  display: flex;
  flex-direction: column;
  gap: 0.2rem;
  min-width: 11rem;
}
.cells {
  display: flex;
  gap: 2px;
  height: 6px;
}
.cell {
  flex: 1;
  border-radius: 2px;
  background: var(--fw-border);
}
.cell.ok {
  background: #2e8b57;
}
.cell.failed {
  background: #c0392b;
}
.cell.blocked {
  background: var(--fw-muted);
}
.cell.running {
  background: #d4a017;
  animation: cell-flash 1s ease-in-out infinite;
}
.cell.stale {
  background: #d4a017;
  opacity: 0.5;
}
@keyframes cell-flash {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.35;
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
/* pending/running with no task board yet: sliding sliver */
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
.task-line {
  display: flex;
  gap: 0.4rem;
  font-size: 0.72rem;
  color: var(--fw-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.task-id {
  color: var(--fw-fg, inherit);
}
.task-detail {
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
