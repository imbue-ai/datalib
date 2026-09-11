<script setup lang="ts">
// A job's state as one bar: full and green when done, red when failed,
// muted when canceled, a sliding sliver while pending or running. What
// the run's steps are doing is on the Manager2 tab, from the run store.
defineProps<{
  msg: string | null;
  state: string;
}>();
</script>

<template>
  <div class="step-progress">
    <div class="single" :class="state">
      <span class="single-fill" />
    </div>
    <div class="step-label" :title="msg ?? state">{{ msg || state }}</div>
  </div>
</template>

<style scoped>
.step-progress {
  display: flex;
  flex-direction: column;
  gap: 0.2rem;
  min-width: 11rem;
}
.single {
  position: relative;
  height: 6px;
  border-radius: 3px;
  background: var(--datalib-border);
  overflow: hidden;
}
.single-fill {
  position: absolute;
  top: 0;
  left: 0;
  height: 100%;
  border-radius: 3px;
  background: var(--datalib-accent);
  width: 100%;
}
.single.done .single-fill {
  background: #2e8b57;
}
.single.failed .single-fill {
  background: #c0392b;
}
.single.canceled .single-fill {
  background: var(--datalib-muted);
}
/* pending/running: sliding sliver */
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
  color: var(--datalib-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
