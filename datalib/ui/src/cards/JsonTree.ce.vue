<script setup lang="ts">
// A JSON value as a tree: objects and arrays fold, scalars show as
// they are. Each entry offers its value to the clipboard and, through
// `pick`, as something to filter by. Recursive by its own name.
import { ref } from "vue";
import { copyToClipboard } from "@/clipboard";

defineOptions({ name: "JsonTree" });

const props = defineProps<{
  value: unknown;
  /// The key this value sits under, or none at the root.
  name?: string | null;
  depth?: number;
}>();

const emit = defineEmits<{
  /// The reader wants lines with (or without) this key and scalar
  /// value.
  (e: "pick", key: string, value: unknown, exclude: boolean): void;
}>();

const open = ref((props.depth ?? 0) < 2);
const copied = ref(false);

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function entries(v: unknown): [string, unknown][] {
  if (Array.isArray(v)) return v.map((x, i) => [String(i), x]);
  if (isObject(v)) return Object.entries(v);
  return [];
}

function scalar(v: unknown): string {
  return typeof v === "string" ? v : JSON.stringify(v);
}

function summary(v: unknown): string {
  if (Array.isArray(v)) return `[${v.length}]`;
  if (isObject(v)) return `{${Object.keys(v).length}}`;
  return "";
}

async function copy() {
  const text = typeof props.value === "string" ? props.value : JSON.stringify(props.value, null, 2);
  copied.value = await copyToClipboard(text);
  setTimeout(() => (copied.value = false), 1200);
}
</script>

<template>
  <div class="jt" :class="{ 'jt-root': name == null }">
    <div class="jt-row">
      <button
        v-if="entries(value).length"
        class="jt-fold"
        type="button"
        :aria-label="open ? 'Fold' : 'Unfold'"
        @click="open = !open"
      >
        {{ open ? "▾" : "▸" }}
      </button>
      <span v-else class="jt-fold jt-leaf" />
      <span v-if="name != null" class="jt-key">{{ name }}</span>
      <span v-if="entries(value).length" class="jt-summary">{{ summary(value) }}</span>
      <span v-else class="jt-value" :class="`jt-${typeof value}`">{{ scalar(value) }}</span>
      <span class="jt-actions">
        <button class="jt-btn" type="button" title="Copy the value" @click="copy">
          {{ copied ? "✓" : "copy" }}
        </button>
        <template v-if="name != null && !entries(value).length">
          <button
            class="jt-btn"
            type="button"
            title="Keep only lines with this value"
            @click="emit('pick', name, value, false)"
          >
            keep
          </button>
          <button
            class="jt-btn"
            type="button"
            title="Exclude lines with this value"
            @click="emit('pick', name, value, true)"
          >
            exclude
          </button>
        </template>
      </span>
    </div>
    <div v-if="open && entries(value).length" class="jt-children">
      <JsonTree
        v-for="[k, v] in entries(value)"
        :key="k"
        :name="k"
        :value="v"
        :depth="(depth ?? 0) + 1"
        @pick="(key, val, ex) => emit('pick', key, val, ex)"
      />
    </div>
  </div>
</template>

<style>
.jt {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  line-height: 1.5;
}
.jt-row {
  display: flex;
  align-items: baseline;
  gap: 6px;
  min-width: 0;
}
.jt-row:hover {
  background: var(--datalib-hover);
}
.jt-fold {
  flex: 0 0 1.1em;
  padding: 0;
  border: 0;
  background: none;
  color: var(--datalib-muted);
  cursor: pointer;
  font: inherit;
}
.jt-leaf {
  cursor: default;
}
.jt-key {
  color: var(--datalib-accent);
  flex: 0 0 auto;
}
.jt-key::after {
  content: ":";
  color: var(--datalib-muted);
}
.jt-summary {
  color: var(--datalib-muted);
}
.jt-value {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  min-width: 0;
}
.jt-number,
.jt-boolean {
  color: var(--datalib-log-ok);
}
.jt-actions {
  margin-left: auto;
  flex: 0 0 auto;
  visibility: hidden;
}
.jt-row:hover .jt-actions {
  visibility: visible;
}
.jt-btn {
  border: 1px solid var(--datalib-border);
  border-radius: 3px;
  background: var(--datalib-bg);
  color: var(--datalib-muted);
  font: inherit;
  font-size: 11px;
  padding: 0 5px;
  cursor: pointer;
}
.jt-btn:hover {
  color: inherit;
}
.jt-children {
  margin-left: 1.1em;
  border-left: 1px solid var(--datalib-border);
  padding-left: 6px;
}
</style>
