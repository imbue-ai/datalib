<script setup lang="ts">
// The run log as a card: the panel, titled by what its pickers show,
// and a line selected in it opened as the card beside this one.
import { computed, onMounted, onUnmounted, ref } from "vue";
import RunLogPanel, { type LogScope } from "@/components/RunLogPanel.ce.vue";
import { logLineSource } from "./libs/logLineView";
import type { LogViewOpts } from "./libs/logView";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; opts: LogViewOpts }>();

const scope = ref<LogScope | null>(null);
const panel = ref<InstanceType<typeof RunLogPanel> | null>(null);
let unsubscribe: (() => void) | null = null;

// The inspector beside this card narrows the log through the bus: a
// token to add to the query bar, as the right-click menu would.
onMounted(() => {
  unsubscribe = props.ctx.bus.subscribe("log.query", (payload) => {
    const token = (payload as { token?: unknown })?.token;
    if (typeof token === "string") panel.value?.addToken(token);
  });
});
onUnmounted(() => unsubscribe?.());

const title = computed(() => {
  const s = scope.value;
  if (s?.kind === "launch") return s.launch.process === "ui" ? "Page log" : "Server log";
  if (s?.kind === "run") {
    const p = s.process;
    if (p?.step) return `Log · ${p.step}`;
    if (p) return "Log · the runner";
    return "Log · the whole run";
  }
  if (s?.kind === "all") return "Log · everything";
  return props.opts.step ? `Log · ${props.opts.step}` : props.opts.launch ? "Server log" : "Log";
});

props.ctx.setTitle(title.value);
props.ctx.setHelp(`
<p>The lines one process wrote — a step's attempt (its own output and
what the runner said about it), the runner, a launch of the server, or
a page of the app (what was done there, as the page reported it) — or
every line of a run. The two pickers move between runs, launches, pages
and processes; the query bar narrows the lines (<code>level:warn
-target:sqlx "a phrase"</code>, <code>min_level:info</code> for a level
and above). Right-click a cell to keep only, or exclude, its value;
drag a column header into the bar above the grid to group by it.</p>
<p>Select a line to open it in full beside this card; the arrow keys
move the selection.</p>
`);

function onScope(s: LogScope) {
  scope.value = s;
  props.ctx.setTitle(title.value);
}

function onSelected(seq: number) {
  props.ctx.host.openCards(logLineSource(seq));
}
</script>

<template>
  <RunLogPanel
    ref="panel"
    :run-id="opts.run ?? '*'"
    :step="opts.step ?? null"
    :launch-id="opts.launch ?? null"
    :initial-query="opts.q"
    :jump-to-end="opts.jumpToEnd"
    @scope-changed="onScope"
    @line-selected="onSelected"
  />
</template>

<style>
:host,
.card-app-root {
  display: flex;
  flex-direction: column;
}
</style>
