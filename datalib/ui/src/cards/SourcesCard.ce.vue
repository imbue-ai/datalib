<script setup lang="ts">
// The Sources card: the tree of what config.toml declares over
// `GET /api/manage/rows`, drawn by TableGrid — a header that says what
// the last sync did, notices as strips above the table, and a status
// column that says its word before its time. Its logic is
// sourcesCardModel.ts.
import { computed, onMounted } from "vue";
import type { Column } from "@slickgrid-universal/common";
import type { CardCtx } from "./types";
import type { StatusView } from "@/api";
import TableGrid from "./TableGrid.ce.vue";
import SourceWizard from "@/components/SourceWizard.vue";
import ConfirmDialog from "@/components/ConfirmDialog.vue";
import { SOURCES_HELP, useSourcesCard, type Row } from "./sourcesCardModel";
import { formatRelative, formatStamp } from "@/config/timeFormat";
import { density } from "@/density";
import { statusTone } from "./dashboard";

// `add`: open on the add-source form, as the Dashboard's "Add source" does.
const props = defineProps<{ ctx: CardCtx; add?: boolean }>();

props.ctx.setTitle("Sources");
props.ctx.setHelp(SOURCES_HELP);

const {
  cardEl,
  banner,
  busy,
  parseError,
  configError,
  loadError,
  configPath,
  droppedRows,
  emptyDiagnosis,
  manage,
  rows,
  syncAll,
  actions,
  openConfig,
  openAdd,
  contextMenuItems,
  isGroupOpenByDefault,
  onGridReady,
  onCellDoubleClicked,
  onCellEdit,
  onRowGroupOpened,
  wizardOpen,
  wizardKey,
  takenIds,
  editing,
  closeWizard,
  onWizardSubmit,
  asking,
} = useSourcesCard(props.ctx);

/// What the header says when no action of this card has anything to
/// say: how the last sync went.
const runLine = computed(() => {
  const run = manage.value?.run;
  if (!run) return { ok: true, text: "Not synced yet" };
  if (run.live) return { ok: true, text: "Syncing now" };
  return { ok: true, text: `Last sync finished ${formatRelative(run.finished_at, Date.now())}` };
});
const headLine = computed(() => banner.value ?? runLine.value);

const configBlocked = computed(() => busy.value || !!parseError.value || !!configError.value);

/// The status as a word first, then when: "Failed · 2 hours ago". The
/// mark carries the word for assistive tech (`role="img"`), and the
/// cell keeps the shared `tg-status*` classes the tests read it by.
function statusCell(s: StatusView | null): HTMLElement {
  const wrap = document.createElement("span");
  if (!s) return wrap;
  const key = s.key.replace(/[\s_]+/g, "-");
  wrap.className = `tg-status tg-status-${key} sx-status sx-tone-${statusTone(s.key)}`;
  wrap.title = s.detail ? `${s.label} — ${s.detail}` : s.label;
  const mark = document.createElement("span");
  mark.className = s.key === "running" ? "tg-spinner sx-spinner" : "sx-dot";
  mark.setAttribute("role", "img");
  mark.setAttribute("aria-label", s.label);
  wrap.appendChild(mark);
  const word = document.createElement("span");
  word.className = "sx-word";
  word.setAttribute("aria-hidden", "true");
  word.textContent = s.label;
  wrap.appendChild(word);
  if (s.at) {
    const when = document.createElement("span");
    when.className = "tg-status-at sx-when";
    when.textContent = formatRelative(s.at, Date.now());
    when.title = formatStamp(s.at);
    wrap.appendChild(when);
  }
  return wrap;
}

const columnOverrides: Record<string, Partial<Column<Row>>> = {
  status: {
    width: 220,
    formatter: (_r, _c, value) => statusCell(value as StatusView | null),
  },
};

onMounted(() => {
  if (props.add) openAdd();
});

// Rows sized to the density; the grid reads its height once, so a
// change rebuilds it.
const rowHeight = computed(() => Math.round(28 + 8 * density.value));
</script>

<template>
  <section ref="cardEl" class="sx">
    <header class="sx-head">
      <p
        class="sx-line"
        :class="headLine.ok ? 'sx-line-ok' : 'sx-line-bad'"
        :title="headLine.text"
        role="status"
      >
        {{ headLine.text }}
      </p>
      <button
        class="sx-btn"
        :class="{ 'sx-btn-danger': syncAll.glyph === 'stop' }"
        :disabled="configBlocked || !!syncAll.blocked"
        :title="syncAll.blocked ?? syncAll.label"
        @click="
          syncAll.stops.length > 0 ? actions.stopSyncs(syncAll.stops) : actions.runEverything()
        "
      >
        {{ syncAll.label }}
      </button>
      <button class="sx-btn sx-btn-primary" :disabled="configBlocked" @click="openAdd">
        Add source
      </button>
    </header>

    <p v-if="loadError" class="sx-strip sx-strip-bad">
      <span class="sx-strip-text">Could not load the config: {{ loadError }}</span>
    </p>
    <p v-if="parseError" class="sx-strip sx-strip-bad">
      <span class="sx-strip-text">
        <b>The config doesn’t parse,</b> so the table below can’t be trusted: {{ parseError }}
      </span>
    </p>
    <div v-else-if="configError" class="sx-strip sx-strip-bad">
      <span class="sx-strip-text">
        <b>datalib won’t run this config.</b> {{ configError }} Nothing syncs, and applets don’t
        start, until it is fixed.
      </span>
      <button class="sx-btn" @click="openConfig">Show the config</button>
    </div>
    <div v-else-if="droppedRows.length" class="sx-strip sx-strip-bad">
      <span class="sx-strip-text">
        <b>
          {{ droppedRows.length }}
          {{ droppedRows.length === 1 ? "entry isn’t" : "entries aren’t" }} in the pipeline.
        </b>
        The rest of the config loaded and still syncs.
        <template v-for="(r, i) in droppedRows" :key="r.id"
          >{{ i ? "; " : "" }}<code>{{ r.id }}</code> — {{ r.dropped?.message }}</template
        >. You can also run <code>datalib-dag --check {{ configPath }}</code
        >.
      </span>
      <button class="sx-btn" @click="openConfig">Show the config</button>
    </div>

    <div class="sx-grid">
      <TableGrid
        :key="rowHeight"
        :columns="manage?.columns ?? []"
        :rows="rows"
        :tree="true"
        :virtualizeRows="false"
        :actions="actions.buttons"
        :menu="contextMenuItems"
        :selectable="true"
        :openByDefault="isGroupOpenByDefault"
        :pinnedColumns="1"
        :columnOverrides="columnOverrides"
        :rowHeight="rowHeight"
        @ready="onGridReady"
        @cellDoubleClick="onCellDoubleClicked"
        @edit="onCellEdit"
        @rowGroupOpened="onRowGroupOpened"
      />
    </div>

    <div v-if="emptyDiagnosis && !parseError" class="sx-strip sx-strip-bad">
      <span class="sx-strip-text">
        <b>This table is empty, and it shouldn’t be.</b> {{ emptyDiagnosis }}
      </span>
      <button class="sx-btn" @click="openConfig">Show the config</button>
    </div>
    <p v-else-if="rows.length === 0 && !parseError" class="sx-empty">
      Nothing configured yet. <b>Add source</b> walks you through one.
    </p>

    <Teleport to="body">
      <SourceWizard
        v-if="wizardOpen"
        :key="wizardKey"
        :taken-ids="takenIds"
        :editing="editing"
        @close="closeWizard"
        @submit="onWizardSubmit"
      />
      <ConfirmDialog
        v-if="asking"
        title="Remove"
        :message="asking.message"
        :check-label="asking.checkLabel"
        confirm-label="Remove"
        @answer="asking.resolve"
      />
    </Teleport>
  </section>
</template>
