<script setup lang="ts">
// One log line in full: what the grid's row clips — the whole message,
// the fields as a tree, where in the source it came from, and the
// process that wrote it. Opened beside the log by a selection there.
import { computed, onMounted, ref, watch } from "vue";
import { fetchLogLine, type ProcessInfo, type RunLogLine } from "@/api";
import { copyToClipboard } from "@/clipboard";
import { formatRelative, formatStamp } from "@/config/timeFormat";
import { filterToken, quoteValue } from "@/grid/query";
import { sourceLabel, sourceOf, sourceUrl, SOURCE_REPO } from "@/components/runLogSource";
import JsonTree from "./JsonTree.ce.vue";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; seq: number }>();

const line = ref<RunLogLine | null>(null);
const process = ref<ProcessInfo | null>(null);
const error = ref<string | null>(null);
const copied = ref(false);

props.ctx.setTitle(`Line ${props.seq}`);
props.ctx.setHelp(`
<p>One line of the log, in full. The fields are what the line carried
beyond its message; hover one for <em>copy</em>, and <em>keep</em> /
<em>exclude</em>, which narrow the log beside this card to lines with
(or without) that value. The source link opens the line of code that
wrote it, at the commit the process was built from.</p>
`);

async function load() {
  error.value = null;
  try {
    const r = await fetchLogLine(props.seq);
    line.value = r.line;
    process.value = r.process;
    // The line's own words are its name; the level says how to read them.
    const words = r.line.msg.split("\n")[0].slice(0, 60);
    props.ctx.setTitle(`${r.line.level} · ${words}`);
  } catch (e) {
    error.value = (e as Error).message;
  }
}

onMounted(load);
watch(() => props.seq, load);

const fields = computed<Record<string, unknown> | null>(() => {
  if (!line.value?.fields) return null;
  try {
    const parsed = JSON.parse(line.value.fields) as unknown;
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
    const rest = { ...(parsed as Record<string, unknown>) };
    // Shown as the source link, not as fields.
    delete rest.filename;
    delete rest.line_number;
    return Object.keys(rest).length ? rest : null;
  } catch {
    return null;
  }
});

const source = computed(() => sourceOf(line.value?.fields));
const sourceHref = computed(() =>
  source.value && line.value?.git_hash ? sourceUrl(line.value.git_hash, source.value) : null,
);
const commitHref = computed(() =>
  line.value?.git_hash ? `${SOURCE_REPO}/commit/${line.value.git_hash}` : null,
);

/// How the process reads: the runner, a step's attempt, the server or
/// a page of the app, and how it ended.
const processLabel = computed(() => {
  const p = process.value;
  if (!p) return line.value?.process ? `${line.value.process} (no longer in the store)` : "unknown";
  const who =
    p.process === "step"
      ? `${p.step ?? "a step"} · attempt ${p.attempt ?? "?"}`
      : p.process === "dag"
        ? "the runner"
        : p.process === "http"
          ? "the server"
          : p.process === "ui"
            ? "a page of the app"
            : p.process;
  const end =
    p.finished_at_utc == null
      ? p.process === "ui"
        ? "open"
        : "running"
      : p.signal != null
        ? `ended by signal ${p.signal}`
        : p.exit_code != null
          ? `exited ${p.exit_code}`
          : "finished";
  return `${who} · ${end}`;
});

/// A filter for the log beside this card: a column's value as its key,
/// a field's as the text the fields column would match.
function narrowBy(key: string, value: string, exclude: boolean) {
  props.ctx.bus.publish(
    "log.query",
    { token: filterToken(key, value, exclude) },
    { from: props.ctx.cardId },
  );
}

function narrowByField(key: string, value: unknown, exclude: boolean) {
  const pair = `"${key}":${JSON.stringify(value)}`;
  props.ctx.bus.publish(
    "log.query",
    { token: `${exclude ? "-" : ""}${quoteValue(pair)}` },
    { from: props.ctx.cardId },
  );
}

async function copyLine() {
  if (!line.value) return;
  copied.value = await copyToClipboard(
    JSON.stringify({ ...line.value, process: process.value }, null, 2),
  );
  setTimeout(() => (copied.value = false), 1200);
}
</script>

<template>
  <section class="ll">
    <p v-if="error" class="ll-empty ll-error">{{ error }}</p>
    <p v-else-if="!line" class="ll-empty">loading…</p>
    <template v-else>
      <header class="ll-head">
        <span class="ll-level" :class="`ll-${line.level}`">{{ line.level }}</span>
        <span v-if="line.target" class="ll-target">
          <button
            class="ll-chip"
            type="button"
            title="Keep only this target"
            @click="narrowBy('target', line.target!, false)"
          >
            {{ line.target }}
          </button>
        </span>
        <span class="ll-when" :title="formatStamp(line.ts_utc)">
          {{ formatStamp(line.ts_utc) }} · {{ formatRelative(line.ts_utc, Date.now()) }}
        </span>
        <button class="ll-copy" type="button" title="Copy the line as JSON" @click="copyLine">
          {{ copied ? "✓ copied" : "copy" }}
        </button>
      </header>

      <pre class="ll-msg">{{ line.msg }}</pre>

      <dl class="ll-meta">
        <template v-if="line.step">
          <dt>step</dt>
          <dd>
            <button
              class="ll-chip"
              type="button"
              title="Keep only this step"
              @click="narrowBy('step', line.step!, false)"
            >
              {{ line.step }}
            </button>
            <span v-if="line.attempt" class="ll-dim"> · attempt {{ line.attempt }}</span>
          </dd>
        </template>
        <dt>process</dt>
        <dd>{{ processLabel }}</dd>
        <template v-if="line.thread">
          <dt>thread</dt>
          <dd>
            <button
              class="ll-chip"
              type="button"
              title="Keep only this thread"
              @click="narrowBy('thread', line.thread!, false)"
            >
              {{ line.thread }}
            </button>
            <span v-if="line.stream" class="ll-dim"> · {{ line.stream }}</span>
          </dd>
        </template>
        <template v-if="source">
          <dt>source</dt>
          <dd>
            <a
              v-if="sourceHref"
              class="ll-link"
              :href="sourceHref"
              target="_blank"
              rel="noopener"
              >{{ sourceLabel(source) }}</a
            >
            <span v-else>{{ sourceLabel(source) }}</span>
          </dd>
        </template>
        <template v-if="line.git_hash">
          <dt>commit</dt>
          <dd>
            <a
              v-if="commitHref"
              class="ll-link"
              :href="commitHref"
              target="_blank"
              rel="noopener"
              >{{ line.git_hash.slice(0, 10) }}</a
            >
          </dd>
        </template>
        <template v-if="line.run_id">
          <dt>run</dt>
          <dd>
            <code>{{ line.run_id }}</code>
          </dd>
        </template>
        <dt>seq</dt>
        <dd>
          <code>{{ line.seq }}</code>
        </dd>
      </dl>

      <template v-if="fields">
        <h4 class="ll-h">Fields</h4>
        <JsonTree :value="fields" @pick="narrowByField" />
      </template>
    </template>
  </section>
</template>

<style>
.ll {
  height: 100%;
  overflow-y: auto;
  padding: 0.75rem 1rem;
  box-sizing: border-box;
  font-size: 13px;
}
.ll-empty {
  color: var(--datalib-muted);
}
.ll-error {
  color: var(--datalib-log-error);
}
.ll-head {
  display: flex;
  align-items: baseline;
  gap: 8px;
  flex-wrap: wrap;
  margin-bottom: 0.5rem;
}
.ll-level {
  font-size: 0.7rem;
  line-height: 1rem;
  padding: 0 0.4rem;
  border-radius: 0.5rem;
  text-transform: uppercase;
  background: color-mix(in srgb, currentColor 12%, transparent);
}
.ll-error,
.ll-level.ll-error {
  color: var(--datalib-log-error);
}
.ll-level.ll-warn {
  color: var(--datalib-log-warn);
}
.ll-level.ll-debug,
.ll-level.ll-trace {
  color: var(--datalib-muted);
}
.ll-target {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.ll-when {
  color: var(--datalib-muted);
  font-size: 12px;
  margin-left: auto;
}
.ll-copy,
.ll-chip {
  border: 1px solid var(--datalib-border);
  border-radius: 3px;
  background: var(--datalib-bg);
  color: inherit;
  font: inherit;
  font-size: 12px;
  padding: 1px 6px;
  cursor: pointer;
}
.ll-chip {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.ll-copy:hover,
.ll-chip:hover {
  background: var(--datalib-hover);
}
.ll-msg {
  margin: 0 0 0.75rem;
  padding: 0.5rem 0.65rem;
  background: var(--datalib-code-bg);
  border-radius: 4px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12.5px;
  line-height: 1.45;
}
.ll-meta {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 4px 12px;
  margin: 0 0 0.75rem;
  font-size: 12.5px;
}
.ll-meta dt {
  color: var(--datalib-muted);
}
.ll-meta dd {
  margin: 0;
  min-width: 0;
  overflow-wrap: anywhere;
}
.ll-dim {
  color: var(--datalib-muted);
}
.ll-link {
  color: inherit;
  text-decoration: underline dotted;
}
.ll-h {
  margin: 0.25rem 0 0.35rem;
  font-size: 0.8rem;
  color: var(--datalib-muted);
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
</style>
