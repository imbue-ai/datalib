<script setup lang="ts">
// One source as one card: its row of the Sources table at the top — its
// chip, status, size and actions — then two tabs. Documents is its browse
// grid (the search, with the columns that suit its type). Sync is each
// step's row under the group's, each with its own toolbar and charts over
// the run — what it has queued and done, the warnings and errors it
// logged, what it weighs — and the group's log at the bottom. The rows
// and their actions are the Sources table's (`GET /api/manage/rows`,
// `rowActions.ts`); the series are `GET /api/manage/groups/{id}/dashboard`.
import { computed, nextTick, onMounted, onUnmounted, ref, watch, type Directive } from "vue";
import { UNIFIED_DOCUMENT_INDEX, type Dashboard, type ManageResponse, type ManageRow } from "@/api";
import { useApi } from "./cardApi";
import { rowMenu, type MenuAction } from "@/config/rowMenu";
import { isDesktopApp, revealActionLabel } from "@/desktop";
import { changed, oneAtATime, subscribeLive } from "@/live";
import { formatShortStamp, formatStamp } from "@/config/timeFormat";
import { formatBytes } from "@/config/bytes";
import RunLogPanel from "@/components/RunLogPanel.ce.vue";
import TimeChart from "./TimeChart.ce.vue";
import { groupCharts, groupSpan, stepCharts, type Chart } from "./dashboardCharts";
import { groupBrowseOpts, menuTarget, rowActions, withBrowse, type ActionRow } from "./rowActions";
import { renderBadges, renderQuantity, renderStatus } from "./cellRenderers";
import { sourceCardOpts, type SourceTab, type SourceViewOpts } from "./cardSources";
import { uriFromEntity } from "./chipLinks";
import EntityChip from "./EntityChip.ce.vue";
import GridCard from "./GridCard.ce.vue";
import { logLineSource } from "./libs/logLineView";
import { TOPIC_CARD_REOPENED, type CardCtx, type CardReopenedPayload } from "./types";

const props = defineProps<{ ctx: CardCtx; opts: SourceViewOpts }>();
const api = useApi();
const group = props.opts.group;

props.ctx.setTitle(group);
props.ctx.setHelp(`
<p>One source — or the index — as one card. The top is the whole group, as
its row in the Sources table reads: its status, how much is <b>queued</b>
across its steps and when that is done (<b>ETA</b>, at the pace work has
come off the queues lately), its size, and the same actions as its row.</p>
<p><b>Documents</b> is what the source holds, as the search shows it, with
the columns that suit its type: search, sort and open them here.</p>
<p><b>Sync</b> has one section per step, each with its own toolbar — the
same actions as its row — and its charts.</p>
<p>The charts cover one run: the newest the group took part in, or the one
picked above. Each metric a step reports is a running total over the run
(API requests, rows written, checkpoints sealed); <b>queued</b> is how
much work was ahead of it; <b>warnings and errors logged</b> counts its
log lines of those levels; <b>size on disk</b> is its tree. Every chart
shares the run's time axis: hover one and the others read the same
instant. While the run goes, the charts follow it.</p>
<p><b>Log</b> at the bottom opens the group's lines from that run, as the
log card shows them; select a line to open it beside this card. Edit,
rename, remove and compare are on the Sources card's row menu.</p>
`);

/// Why this card does not edit a row: the form needs the config's text,
/// which the Sources card holds.
const EDIT_ELSEWHERE = "Edit settings from the Sources card";
/// Menu entries this card leaves out: the ones that need that text, and
/// the one that opens this card.
const NOT_HERE: MenuAction[] = ["edit", "rename", "remove", "compare", "dashboard"];

const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

// ── State ────────────────────────────────────────────────────────────

type State = { run: string | null; logOpen: boolean; tab: SourceTab | null; docs: string };

function readState(): State {
  try {
    const s = JSON.parse(props.ctx.initialState || "{}");
    return {
      run: typeof s.run === "string" ? s.run : null,
      logOpen: !!s.logOpen,
      tab: s.tab === "documents" || s.tab === "sync" ? s.tab : null,
      docs: typeof s.docs === "string" ? s.docs : "",
    };
  } catch {
    return { run: null, logOpen: false, tab: null, docs: "" };
  }
}
const initial = readState();
/// The run picked, or null to follow the newest.
const pickedRun = ref<string | null>(initial.run);
const logOpen = ref(initial.logOpen);
/// The tab asked for: kept from last time, else what the card source
/// says — a step asks for its section of Sync — else the documents.
const tab = ref<SourceTab>(
  initial.tab ?? props.opts.tab ?? (props.opts.step ? "sync" : "documents"),
);
/// The documents grid's own state (its query, columns, selection), kept
/// inside this card's.
const docsState = ref(initial.docs);
watch([pickedRun, logOpen, tab, docsState], ([run, open, t, docsKept]) => {
  props.ctx.host.setState(JSON.stringify({ run, logOpen: open, tab: t, docs: docsKept }));
});

const manage = ref<ManageResponse | null>(null);
const dash = ref<Dashboard | null>(null);
const loadError = ref<string | null>(null);
const banner = ref<{ ok: boolean; text: string } | null>(null);
const busy = ref(false);
/// uPlot's cursor sync is page-wide by key: this card's charts share one
/// crosshair, and another dashboard's are not in it.
const syncKey = `sd-${group}-${props.ctx.cardId}`;
const now = ref(Date.now());

const rows = computed<ActionRow[]>(() => {
  const all = manage.value?.rows ?? [];
  const groups = new Map<string, ManageRow>(
    all.filter((r) => r.kind === "group").map((g) => [g.id, g]),
  );
  return all.map((r) => withBrowse(r, groups, canReveal));
});
const groupRow = computed(() => rows.value.find((r) => r.kind === "group" && r.id === group));
const stepRows = computed(() => rows.value.filter((r) => r.kind === "step" && r.group === group));

watch(groupRow, (g) => {
  if (g) props.ctx.setTitle(g.name.label);
});

/// What the Documents tab shows: the source's Browse, with the columns
/// that suit its type; null for a group that has no documents, whose card
/// is its sync alone.
const docs = computed(() => (groupRow.value ? groupBrowseOpts(groupRow.value) : null));
const shownTab = computed<SourceTab>(() => (docs.value ? tab.value : "sync"));
/// Whether the Documents tab has shown yet: its grid mounts then.
const docsSeen = ref(false);
watch(
  shownTab,
  (t) => {
    if (t === "documents") docsSeen.value = true;
  },
  { immediate: true },
);

/// The documents grid is a card of its own drawn inside this one: it opens
/// what it opens beside this card, keeps its state inside this card's, and
/// leaves the title and help to this card.
const docsCtx: CardCtx = {
  cardId: `${props.ctx.cardId}:documents`,
  cardType: "searchView",
  initialState: initial.docs,
  setTitle: () => {},
  setHelp: () => {},
  bus: props.ctx.bus,
  host: {
    openCards: (...sources) => props.ctx.host.openCards(...sources),
    hrefFor: (...sources) => props.ctx.host.hrefFor(...sources),
    setState: (state) => (docsState.value = state),
    setSource: () => {},
    becomeComposite: () => {},
    close: () => {},
  },
};

const actions = rowActions<ActionRow>({
  api,
  host: props.ctx.host,
  rows: () => rows.value,
  run: () => manage.value?.run ?? null,
  say: (ok, text) => (banner.value = { ok, text }),
  clear: () => (banner.value = null),
  busy,
  reload: async (fresh) => {
    await Promise.all([loadRows(fresh), loadDashboard()]);
  },
});

function toolbar(row: ActionRow) {
  return rowMenu([menuTarget(row, EDIT_ELSEWHERE)], { canReveal, revealLabel }).filter(
    (e) => e.separator || !NOT_HERE.includes(e.action),
  );
}

async function runEntry(action: MenuAction, row: ActionRow) {
  await actions.runMenuAction(action, [row]);
}

// ── The run and its time axis ────────────────────────────────────────

const run = computed(() => dash.value?.run ?? null);
const domain = computed<[number, number]>(() =>
  groupSpan(dash.value?.steps ?? [], run.value, now.value),
);

/// Where a step's lines stop: when it finished, or the run's end.
function endOf(stepId: string | null): number {
  const s = dash.value?.steps.find((x) => x.id === stepId);
  if (s?.finished_at_utc) return Math.min(Date.parse(s.finished_at_utc), domain.value[1]);
  return domain.value[1];
}

const panels = computed(() => {
  const d = dash.value;
  const byId = new Map((d?.steps ?? []).map((s) => [s.id, s]));
  return stepRows.value.map((row) => {
    const step = byId.get(row.id) ?? null;
    return { row, step, charts: step?.in_run ? stepCharts(step) : ([] as Chart[]) };
  });
});
const topCharts = computed(() =>
  dash.value ? groupCharts(dash.value.steps, dash.value.disk) : [],
);
const logCount = (s: { warnings: unknown[]; errors: unknown[] } | null) =>
  s ? { warnings: s.warnings.length, errors: s.errors.length } : null;

function runLabel(r: { started_at_utc: string; finished_at_utc: string | null }): string {
  const when = formatShortStamp(r.started_at_utc);
  return r.finished_at_utc ? when : `${when} (running)`;
}

// ── Loading ──────────────────────────────────────────────────────────

let rowsSeq = 0;
async function loadRows(fresh = false) {
  const seq = ++rowsSeq;
  try {
    const m = await api.fetchManageRows(fresh);
    if (seq === rowsSeq) manage.value = m;
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}

let dashSeq = 0;
async function loadDashboard() {
  const seq = ++dashSeq;
  try {
    const d = await api.fetchGroupDashboard(group, pickedRun.value);
    if (seq !== dashSeq) return;
    dash.value = d;
    loadError.value = null;
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}
watch(pickedRun, () => void loadDashboard());

/// The series move many times a second during a run; the charts need
/// not. One refetch at a time, at most every two seconds.
const DASHBOARD_EVERY_MS = 2000;
let dashTimer: ReturnType<typeof setTimeout> | null = null;
let dashLast = 0;
function scheduleDashboard() {
  if (dashTimer) return;
  const wait = Math.max(0, dashLast + DASHBOARD_EVERY_MS - Date.now());
  dashTimer = setTimeout(async () => {
    dashTimer = null;
    dashLast = Date.now();
    await loadDashboard();
  }, wait);
}

// ── Cells drawn by the table's renderers ─────────────────────────────

/// Puts a cell renderer's element in place, as the table draws it.
const vCell: Directive<HTMLElement, HTMLElement> = {
  mounted: (el, b) => el.replaceChildren(b.value),
  updated: (el, b) => el.replaceChildren(b.value),
};

const cardEl = ref<HTMLElement | null>(null);

/// Bring a step's section into view and keep it there while the card
/// settles: a tab the host has just revealed can be hidden or unsized for
/// a few frames, and charts loading above the section push it down. It
/// scrolls again whenever the section moves, until it has stayed put for
/// ten frames, about two seconds have passed, or the person scrolls,
/// clicks or types in the card.
function keepStepInView(id: string) {
  const root = cardEl.value;
  if (!root) return;
  let framesLeft = 120;
  let stillFor = 0;
  let lastTop: number | null = null;
  const stop = () => (framesLeft = 0);
  const inputs = ["wheel", "pointerdown", "keydown"] as const;
  for (const ev of inputs) root.addEventListener(ev, stop, { once: true });
  const tick = () => {
    const section = root.querySelector(`[data-step="${CSS.escape(id)}"]`);
    if (section && section.getClientRects().length > 0) {
      const top = section.getBoundingClientRect().top;
      if (lastTop === null || Math.abs(top - lastTop) > 1) {
        section.scrollIntoView({ block: "start" });
        lastTop = section.getBoundingClientRect().top;
        stillFor = 0;
      } else {
        stillFor++;
      }
    }
    if (stillFor < 10 && --framesLeft > 0) requestAnimationFrame(tick);
    else for (const ev of inputs) root.removeEventListener(ev, stop);
  };
  tick();
}

/// The step a reopen asked for, until its section is drawn: a card still
/// loading its rows has none yet.
const wantedStep = ref<string | null>(null);
watch([wantedStep, panels], async ([id]) => {
  if (!id) return;
  if (!panels.value.some((p) => p.row.id === id)) {
    // Loaded, and this group has no such step: nothing to show.
    if (manage.value) wantedStep.value = null;
    return;
  }
  wantedStep.value = null;
  await nextTick();
  keepStepInView(id);
});

/// Show what a source's card source asks of this card: a step's section
/// of Sync, or a tab.
function show(asked: SourceViewOpts) {
  if (asked.step) {
    tab.value = "sync";
    wantedStep.value = asked.step;
  } else if (asked.tab) {
    tab.value = asked.tab;
  }
}

/// Opened again while open: show what the new open asks for.
const stopReopened = props.ctx.bus.subscribe(TOPIC_CARD_REOPENED, (payload) => {
  const p = payload as CardReopenedPayload;
  const asked = p.cardId === props.ctx.cardId ? sourceCardOpts(p.source) : null;
  if (asked) show(asked);
});

/// A chip on this card asking for a card: this source's card is this one,
/// so its own chips show their part of it here; anything else opens beside.
function openFromChip(source: string) {
  const asked = sourceCardOpts(source);
  if (asked?.group === group) show(asked);
  else props.ctx.host.openCards(source);
}
let unsubscribe: (() => void) | null = null;
let clock: ReturnType<typeof setInterval> | null = null;
/// A reload a frame asks for, one at a time: the loop sends them in bursts.
const refreshRows = oneAtATime(() => loadRows());

onMounted(async () => {
  // Listen before the first load: a card closed while that load is out
  // has already run its unmount, and would never stop listening.
  clock = setInterval(() => {
    if (dash.value?.live) now.value = Date.now();
  }, 1000);
  unsubscribe = subscribeLive(
    {
      root: (e) => {
        if (changed(e, "manage.rows")) refreshRows();
        if (changed(e, "runs") || changed(e, "storage")) scheduleDashboard();
        if (e.kind === "config_changed") {
          void loadRows();
          void loadDashboard();
        }
      },
      resync: () => {
        void loadRows(true);
        void loadDashboard();
      },
    },
    { onScreen: cardEl.value ?? undefined },
  );
  // Draw from the server's last measurement, then walk the disk for
  // fresh sizes: a walk of a large root takes seconds, and the card
  // would stand blank for all of them.
  await Promise.all([loadRows(), loadDashboard()]);
  void loadRows(true);
  if (props.opts.step) wantedStep.value = props.opts.step;
});

onUnmounted(() => {
  stopReopened();
  unsubscribe?.();
  if (clock) clearInterval(clock);
  if (dashTimer) clearTimeout(dashTimer);
});
</script>

<template>
  <section ref="cardEl" class="sd">
    <p v-if="loadError" class="sd-msg bad">Could not load this sync: {{ loadError }}</p>
    <p v-if="banner" class="sd-msg" :class="banner.ok ? 'good' : 'bad'" role="status">
      {{ banner.text }}
    </p>

    <p v-if="manage && !groupRow" class="sd-msg bad">
      The config has no group <code>{{ group }}</code
      >.
    </p>

    <!-- The group: its row, read across its steps. -->
    <article v-if="groupRow" class="sd-section sd-group" :data-step="group">
      <header class="sd-head">
        <span class="sd-name">
          <EntityChip
            :uri="uriFromEntity('group', group)"
            :shown="groupRow.name.label"
            quiet
            :open-card="openFromChip"
          />
          <span
            v-if="groupRow.problems.length"
            v-cell="renderBadges('problems', groupRow.problems)"
            title="Double-click the counts for the problems"
            @dblclick="actions.openProblems(groupRow, `${UNIFIED_DOCUMENT_INDEX}/problems`)"
          />
        </span>
        <span v-cell="renderStatus(groupRow.status)" class="sd-status" />
      </header>
      <dl class="sd-stats">
        <div>
          <dt>ETA</dt>
          <dd v-cell="renderQuantity(groupRow.eta)" />
        </div>
        <div v-if="groupRow.items.value !== null" :title="groupRow.items.detail ?? ''">
          <dt>Items</dt>
          <dd>{{ groupRow.items.value.toLocaleString() }}</dd>
        </div>
        <div v-if="groupRow.disk.value !== null" :title="groupRow.disk.detail ?? ''">
          <dt>Size</dt>
          <dd>{{ formatBytes(groupRow.disk.value) }}</dd>
        </div>
      </dl>
      <nav class="sd-tools" aria-label="Group actions">
        <template v-for="(e, i) in toolbar(groupRow)" :key="i">
          <span v-if="e.separator" class="sd-tools-gap" />
          <button
            v-else
            class="sd-btn"
            :class="{ danger: e.action === 'reset' || e.action === 'stop' }"
            :disabled="busy || !!e.disabled"
            :title="e.disabled ?? e.name"
            @click="runEntry(e.action, groupRow)"
          >
            {{ e.name }}
          </button>
        </template>
      </nav>
    </article>

    <!-- What the source holds, and how it syncs: one tab each. A group
         with no documents is its sync alone, with no tabs. -->
    <nav v-if="groupRow && docs" class="sd-tabs" role="tablist" aria-label="Source">
      <button
        role="tab"
        class="sd-tab"
        :aria-selected="shownTab === 'documents'"
        @click="tab = 'documents'"
      >
        Documents
      </button>
      <button role="tab" class="sd-tab" :aria-selected="shownTab === 'sync'" @click="tab = 'sync'">
        Sync
      </button>
    </nav>

    <!-- Mounted the first time it shows, then kept: the grid sizes itself
         to a box it can measure, and keeps its query when hidden. -->
    <div v-if="docs && docsSeen" v-show="shownTab === 'documents'" class="sd-docs">
      <GridCard
        :ctx="docsCtx"
        :q="docs.q"
        :columns="docs.columns"
        :name="docs.name"
        :view="docs.view"
        one-source
      />
    </div>

    <div v-show="shownTab === 'sync'" class="sd-sync">
      <!-- The run the charts cover, and the whole group's charts over it. -->
      <section v-if="groupRow" class="sd-section sd-run-charts">
        <dl v-if="dash" class="sd-stats">
          <div v-if="dash">
            <dt>Run</dt>
            <dd>
              <select
                class="sd-run"
                :value="pickedRun ?? ''"
                aria-label="Which run the charts show"
                @change="pickedRun = ($event.target as HTMLSelectElement).value || null"
              >
                <option value="">
                  Newest{{ dash.run && !pickedRun ? ` — ${runLabel(dash.run)}` : "" }}
                </option>
                <option v-for="r in dash.runs" :key="r.run_id" :value="r.run_id">
                  {{ runLabel(r) }}
                </option>
              </select>
            </dd>
          </div>
        </dl>
        <p v-if="dash && !dash.run" class="sd-empty">No step of this group has run yet.</p>
        <div v-else class="sd-charts">
          <TimeChart
            v-for="c in topCharts"
            :key="c.key"
            :chart="c"
            :domain="domain"
            :end="domain[1]"
            :sync-key="syncKey"
          />
        </div>
      </section>

      <!-- Each step, as its row. -->
      <article
        v-for="{ row, step, charts } in panels"
        :key="row.id"
        class="sd-section"
        :data-step="row.id"
      >
        <header class="sd-head">
          <span class="sd-name">
            <EntityChip
              :uri="uriFromEntity('step', row.id)"
              :shown="row.name.label"
              short
              quiet
              :open-card="openFromChip"
            />
          </span>
          <span v-cell="renderStatus(row.status)" class="sd-status" />
          <span class="sd-inline">
            <span class="sd-inline-key">Queue</span>
            <span v-cell="renderQuantity(row.queue)" />
            <span class="sd-inline-key">ETA</span>
            <span v-cell="renderQuantity(row.eta)" />
            <template v-if="row.disk.value !== null">
              <span class="sd-inline-key">Size</span>
              <span :title="row.disk.detail ?? ''">{{ formatBytes(row.disk.value) }}</span>
            </template>
          </span>
        </header>
        <nav class="sd-tools" :aria-label="`${row.name.label} actions`">
          <template v-for="(e, i) in toolbar(row)" :key="i">
            <span v-if="e.separator" class="sd-tools-gap" />
            <button
              v-else
              class="sd-btn"
              :class="{ danger: e.action === 'reset' || e.action === 'stop' }"
              :disabled="busy || !!e.disabled"
              :title="e.disabled ?? e.name"
              @click="runEntry(e.action, row)"
            >
              {{ e.name }}
            </button>
          </template>
        </nav>
        <p v-if="step?.msg && step.state === 'running'" class="sd-note">{{ step.msg }}</p>
        <p v-if="step?.error" class="sd-note bad">{{ step.error }}</p>
        <p v-if="!step || !step.in_run" class="sd-empty">Not in this run.</p>
        <template v-else>
          <p class="sd-note">
            {{ step.state }}
            <template v-if="step.started_at_utc">
              · started {{ formatStamp(step.started_at_utc) }}</template
            >
            <template v-if="step.finished_at_utc">
              · ended {{ formatStamp(step.finished_at_utc) }}</template
            >
            <template v-if="(step.attempt ?? 1) > 1"> · attempt {{ step.attempt }}</template>
            <template v-if="logCount(step)?.warnings === 0 && logCount(step)?.errors === 0">
              · no warnings or errors logged</template
            >
          </p>
          <div class="sd-charts">
            <TimeChart
              v-for="c in charts"
              :key="c.key"
              :chart="c"
              :domain="domain"
              :end="endOf(row.id)"
              :sync-key="syncKey"
            />
          </div>
          <p v-if="charts.length === 0" class="sd-empty">
            This step reported no numbers in this run.
          </p>
        </template>
      </article>

      <!-- The group's lines from the run shown. Mounted only while open:
         it tails the store for as long as it is. -->
      <details
        v-if="run"
        class="sd-section sd-log"
        :open="logOpen"
        @toggle="logOpen = ($event.target as HTMLDetailsElement).open"
      >
        <summary>Log · this group, this run</summary>
        <div v-if="logOpen" class="sd-log-body">
          <RunLogPanel
            :key="run.run_id"
            :run-id="run.run_id"
            :step="null"
            :initial-query="`group:${group}`"
            @line-selected="(seq: number) => ctx.host.openCards(logLineSource(seq))"
            @open-card="(source: string) => ctx.host.openCards(source)"
          />
        </div>
      </details>
    </div>
  </section>
</template>

<style>
:host,
.card-app-root {
  display: flex;
  flex-direction: column;
  min-height: 0;
}
/* The card fills its box: the group's strip and the tabs stay put, the
   documents grid takes the rest, and the sync scrolls on its own. */
.sd {
  height: 100%;
  box-sizing: border-box;
  padding: 10px 12px 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  color: var(--datalib-fg);
  font-size: var(--datalib-font-size);
}
.sd > * {
  flex: none;
}
.sd-docs {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
  flex-direction: column;
  margin: 0 -12px;
}
.sd-sync {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding-bottom: 16px;
}
.sd-tabs {
  display: flex;
  gap: 4px;
  border-bottom: 1px solid var(--datalib-border);
}
.sd-tab {
  font: inherit;
  color: var(--datalib-muted);
  background: none;
  border: 0;
  border-bottom: 2px solid transparent;
  padding: 4px 10px;
  cursor: pointer;
}
.sd-tab[aria-selected="true"] {
  color: var(--datalib-fg);
  border-bottom-color: var(--datalib-accent);
}
.sd-section {
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 2px);
  background: var(--datalib-bg);
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.sd-group {
  border-width: 2px;
}
.sd-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px 14px;
}
.sd-name {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  font-weight: 600;
  font-size: var(--datalib-title-size);
}
.sd-status {
  display: inline-flex;
}
.sd-inline {
  display: inline-flex;
  align-items: baseline;
  gap: 6px;
  margin-left: auto;
  font-variant-numeric: tabular-nums;
}
.sd-inline-key {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
  margin-left: 6px;
}
.sd-stats {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 22px;
  margin: 0;
}
.sd-stats div {
  display: flex;
  flex-direction: column;
}
.sd-stats dt {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.sd-stats dd {
  margin: 0;
  font-size: var(--datalib-title-size);
  font-variant-numeric: tabular-nums;
}
.sd-run {
  font: inherit;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-fg);
  background: var(--datalib-input-bg);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  max-width: 260px;
}
.sd-tools {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px;
}
.sd-tools-gap {
  width: 8px;
}
.sd-btn {
  font: inherit;
  font-size: var(--datalib-font-size-small);
  padding: 2px 8px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  cursor: pointer;
}
.sd-btn:hover:not(:disabled) {
  background: var(--datalib-hover);
}
.sd-btn:disabled {
  opacity: 0.45;
  cursor: default;
}
.sd-btn.danger:hover:not(:disabled) {
  border-color: var(--datalib-log-error);
  color: var(--datalib-log-error);
}
.sd-charts {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
  gap: 8px;
}
.sd-note {
  margin: 0;
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.sd-empty {
  margin: 0;
  color: var(--datalib-muted);
  font-style: italic;
  font-size: var(--datalib-font-size-small);
}
.sd-msg {
  margin: 0;
  padding: 6px 10px;
  border-radius: var(--datalib-radius);
  font-size: var(--datalib-font-size-small);
}
.bad {
  color: var(--datalib-log-error);
}
.good {
  color: var(--datalib-log-ok);
}
.sd-log summary {
  cursor: pointer;
  font-weight: 600;
}
.sd-log-body {
  height: 420px;
  display: flex;
  flex-direction: column;
  margin-top: 8px;
}
</style>
