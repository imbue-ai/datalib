<script setup lang="ts">
// `historyView({ trees, title, source, compare })`: every commit in the
// doltlite stores under some trees — one per sync, checkpoint or render
// pass — as a tree of store, commit and table, re-read whenever the
// runner's record moves. Opened from a Manage row's menu. On a source,
// two commits of its download's store can be compared: the comparison
// is a diff group added to the config and synced.
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import type { Column } from "@slickgrid-universal/common";
import type { ColumnSpec } from "@/api";
import { useApi } from "@/cards/cardApi";
import { copyToClipboard } from "@/clipboard";
import { historyRows, truncatedStores, type HistoryRow } from "@/config/commitHistory";
import {
  addComparison,
  comparisonId,
  defaultPair,
  selectedPair,
  type CommitPair,
} from "@/config/compareCommits";
import { formatRelative, formatStamp } from "@/config/timeFormat";
import type { MenuEntry } from "@/grid/menu";
import { changed, subscribeLive } from "@/live";
import { pushToast } from "@/toasts";
import TableGrid from "./TableGrid.ce.vue";
import { logSource } from "./libs/logView";
import type { HistoryViewOpts } from "./libs/historyView";
import { TOPIC_CONFIG_WRITTEN, type CardCtx } from "./types";

const { fetchTreeHistory, fetchConfig, saveConfig, openRequest } = useApi();

const props = defineProps<{ ctx: CardCtx; opts: HistoryViewOpts }>();

props.ctx.setTitle(`History · ${props.opts.title}`);
props.ctx.setHelp(`
<p>Every commit in the stores under ${props.opts.trees.map((t) => `<code>${t}/</code>`).join(", ")},
newest first. Each store is a doltlite database that keeps its own log: a download commits
as it syncs, a render once per pass. Open a commit for what it did to each table; the
<b>Run</b> link opens the log of the sync that made it.</p>
<p>On a source, select two commits of its download's store (⌘-click the second) and
right-click to <b>compare</b> them. A comparison is a source of its own: every record that
was added, removed or changed between the two, as documents with the changes marked. It
stays as it is until you compare again or remove it.</p>
`);

const lines = ref<HistoryRow[]>([]);
const truncated = ref<string[]>([]);
const busy = ref(true);
const error = ref<string | null>(null);
const cardEl = ref<HTMLElement | null>(null);

/// Only the newest read lands: a `dag` frame can arrive while the last
/// read is still out, and answers can come back in either order.
let issued = 0;
let landed = 0;
async function load() {
  const seq = ++issued;
  let answer: { rows: HistoryRow[]; truncated: string[] } | Error;
  try {
    const hs = await Promise.all(props.opts.trees.map((t) => fetchTreeHistory(t)));
    answer = { rows: historyRows(hs), truncated: truncatedStores(hs) };
  } catch (e) {
    answer = e as Error;
  }
  if (seq <= landed) return;
  landed = seq;
  busy.value = false;
  if (answer instanceof Error) {
    error.value = answer.message;
    return;
  }
  error.value = null;
  lines.value = answer.rows;
  truncated.value = answer.truncated;
  if (props.opts.compare && props.opts.source && !offeredDefault) {
    offeredDefault = true;
    startCompare(defaultPair(answer.rows, props.opts.source));
  }
}

const storeNote = computed(() =>
  props.opts.trees.length === 1 && props.opts.trees[0].includes("/")
    ? `the stores in ${props.opts.trees[0]}/`
    : `every store under ${props.opts.trees.map((t) => `${t}/`).join(", ")}`,
);

/// A commit names its run, and that run's log is the "how" behind the
/// commit's "what" — filtered to the step that writes the store.
function openRunLog(row: HistoryRow) {
  if (!row.run) return;
  props.ctx.host.openCards(logSource({ run: row.run, step: row.stepId }));
}

// ── Compare. The pair being compared, or why the pair asked for is not
// one; null while no comparison is being set up.
const comparing = ref<CommitPair | string | null>(null);
const compareName = ref("");
const maxDocuments = ref(1000);
const creating = ref(false);
/// Opened by "Compare two versions…", the card starts on the newest two
/// commits, once; a later read leaves a cancelled compare cancelled.
let offeredDefault = false;

function startCompare(pair: CommitPair | string) {
  comparing.value = pair;
  if (!compareName.value) compareName.value = `${props.opts.title} · changes`;
}

const comparePair = computed(() =>
  comparing.value && typeof comparing.value !== "string" ? comparing.value : null,
);

const canCreate = computed(
  () =>
    !!comparePair.value &&
    !creating.value &&
    compareName.value.trim() !== "" &&
    maxDocuments.value >= 1,
);

/// How a commit reads in the compare bar: when, and what it said.
function commitLabel(c: HistoryRow): string {
  return `${c.date ? formatStamp(c.date) : ""} — ${c.label} (${(c.hash ?? "").slice(0, 8)})`;
}

async function createComparison() {
  const pair = comparePair.value;
  const source = props.opts.source;
  if (!pair || !source || !canCreate.value) return;
  creating.value = true;
  try {
    const cfg = await fetchConfig();
    const name = compareName.value.trim();
    const { text, renderId } = addComparison(cfg.text, {
      id: comparisonId(cfg.text, source, name),
      name,
      source,
      from: pair.from.hash!,
      to: pair.to.hash!,
      maxDocuments: Math.floor(maxDocuments.value),
    });
    const res = await saveConfig(text);
    if (!res.ok) throw new Error(res.error ?? "The config was rejected.");
    props.ctx.bus.publish(TOPIC_CONFIG_WRITTEN, null);
    await openRequest([renderId]);
    pushToast(`Added ${name}, and queued its first sync.`, "info");
    comparing.value = null;
    compareName.value = "";
  } catch (e) {
    pushToast((e as Error).message);
  } finally {
    creating.value = false;
  }
}

function menu(anchor: HistoryRow, targets: HistoryRow[]): MenuEntry[] {
  const source = props.opts.source;
  const pair = source ? selectedPair(lines.value, source, targets) : null;
  const entries: MenuEntry[] = [];
  if (source) {
    entries.push({
      name: "Compare these two versions…",
      disabled: typeof pair === "string" ? pair : null,
      action: () => pair && typeof pair !== "string" && startCompare(pair),
    });
  }
  entries.push({
    name: "Show the run's log",
    disabled: anchor.run ? null : "No run is named on this commit",
    action: () => openRunLog(anchor),
  });
  entries.push({
    name: "Copy the commit hash",
    disabled: anchor.level === "table" || anchor.hash === null ? "Not a commit" : null,
    action: () => void copyToClipboard(anchor.hash ?? ""),
  });
  return entries;
}

/// The grid's columns: what each is, by type, and how the ones a type
/// cannot draw alone are drawn.
const historyColumns: ColumnSpec[] = [
  // The tree column: a store, the commits under it, the tables under
  // each commit. The label is the store's file name, the commit's
  // message, or the table's name; the level says which it is.
  { field: "label", header: "Commit", type: "text", default_visible: true, editable: false },
  // Relative on top, exact underneath — stacked like the size cell,
  // because a sync commits several times inside one minute and ten
  // "18 hours ago"s in a row say nothing about their order.
  { field: "date", header: "When", type: "timestamp", default_visible: true, editable: false },
  {
    field: "rows",
    header: "Rows",
    type: "count",
    description: "Rows after this commit — across the data tables, or in the one table",
    default_visible: true,
    editable: false,
  },
  { field: "added", header: "Added", type: "count", default_visible: true, editable: false },
  { field: "deleted", header: "Deleted", type: "count", default_visible: true, editable: false },
  { field: "modified", header: "Modified", type: "count", default_visible: true, editable: false },
  // The run that made the commit, when the message names one, as the
  // way to its log: the commit is what the run did, the log is how.
  { field: "run", header: "Run", type: "text", default_visible: true, editable: false },
  { field: "hash", header: "Hash", type: "text", default_visible: true, editable: false },
];

const historyOverrides: Record<string, Partial<Column<HistoryRow>>> = {
  label: {
    width: 360,
    params: {
      innerFormatter: (_r: number, _c: number, _v: unknown, _col: unknown, row: HistoryRow) => {
        const wrap = document.createElement("span");
        wrap.className = `hc-label hc-${row?.level ?? "commit"}`;
        wrap.textContent = row?.label ?? "";
        if (row?.level === "store") {
          const dir = document.createElement("span");
          dir.className = "hc-dir";
          dir.textContent = row.storePath.slice(0, row.storePath.lastIndexOf("/"));
          wrap.appendChild(dir);
        }
        return wrap;
      },
    },
  },
  date: {
    width: 170,
    formatter: (_r, _c, value) => {
      const wrap = document.createElement("span");
      if (!value) return wrap;
      wrap.className = "hc-when";
      const rel = document.createElement("span");
      rel.textContent = formatRelative(String(value), Date.now());
      const abs = document.createElement("span");
      abs.className = "hc-dir";
      abs.textContent = formatStamp(String(value));
      wrap.append(rel, abs);
      return wrap;
    },
  },
  rows: {
    width: 100,
    formatter: (_r, _c, value) => formatCount(value as number | null),
  },
  added: {
    width: 90,
    formatter: (_r, _c, value) => formatDelta(value as number | null, "+"),
  },
  deleted: {
    width: 90,
    formatter: (_r, _c, value) => formatDelta(value as number | null, "−"),
  },
  modified: {
    width: 96,
    formatter: (_r, _c, value) => formatDelta(value as number | null, "~"),
  },
  run: {
    width: 120,
    formatter: (_r, _c, _v, _col, row) => {
      const wrap = document.createElement("span");
      if (!row?.run) return wrap;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "hc-run";
      btn.textContent = row.run.slice(0, 8);
      btn.title = `Show the log of run ${row.run}`;
      btn.addEventListener("click", () => void openRunLog(row));
      wrap.appendChild(btn);
      return wrap;
    },
  },
  hash: {
    width: 130,
    formatter: (_r, _c, value, _col, row) => {
      const wrap = document.createElement("span");
      if (row?.level !== "commit" || !value) return { html: wrap, toolTip: "" };
      const hash = String(value);
      wrap.className = "hc-hash";
      wrap.textContent = hash.slice(0, 10);
      wrap.appendChild(copyIdButton(hash, "Copy the commit hash"));
      return { html: wrap, toolTip: hash };
    },
  },
};

/// The 🆔 button the chat views put beside every uuid, for a commit
/// hash: the full 40 characters, where the cell shows ten.
function copyIdButton(id: string, label: string): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "hc-copy-id";
  btn.title = `${label} (${id})`;
  btn.setAttribute("aria-label", label);
  btn.textContent = "🆔";
  btn.addEventListener("click", async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    if (await copyToClipboard(id)) {
      btn.textContent = "✓";
      btn.classList.add("copied");
    } else {
      btn.classList.add("copy-failed");
    }
    setTimeout(() => {
      btn.textContent = "🆔";
      btn.classList.remove("copied", "copy-failed");
    }, 900);
  });
  return btn;
}

const COUNT_FMT = new Intl.NumberFormat();
function formatCount(n: number | null | undefined): string {
  return typeof n === "number" ? COUNT_FMT.format(n) : "";
}
/// A zero reads as nothing rather than as "0": a column of zeros with
/// the odd number in it is easier to scan than a column of numbers.
function formatDelta(n: number | null | undefined, sign: string): string {
  return n ? `${sign}${COUNT_FMT.format(n)}` : "";
}

let unsubscribe: (() => void) | null = null;
onMounted(() => {
  void load();
  unsubscribe = subscribeLive(
    {
      // The loop's record moving is the nearest thing to "a step
      // committed" — nothing watches the stores themselves.
      root: (e) => {
        if (changed(e, "dag")) void load();
      },
      resync: () => void load(),
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => unsubscribe?.());
</script>

<template>
  <div ref="cardEl" class="hc">
    <header class="hc-head">
      <p>
        Each commit in {{ storeNote }}, newest first; open one for what it did to each table.
        <span v-if="opts.source">Select two commits and right-click to compare them.</span>
        <span v-if="truncated.length">
          Only the newest commits are shown for <code>{{ truncated.join(", ") }}</code
          >.
        </span>
        <!-- A failed refresh leaves the log already read in place, and
             whatever was opened in it. -->
        <span v-if="error && lines.length" class="bad">
          The last refresh failed ({{ error }}); this is the log as last read.
        </span>
      </p>
    </header>

    <form v-if="comparing !== null" class="hc-compare" @submit.prevent="createComparison">
      <template v-if="comparePair">
        <div class="hc-compare-pair">
          <span>Compare</span>
          <span class="hc-pick">{{ commitLabel(comparePair.from) }}</span>
          <span>→</span>
          <span class="hc-pick">{{ commitLabel(comparePair.to) }}</span>
        </div>
        <label>
          Name
          <input v-model="compareName" class="hc-name" type="text" />
        </label>
        <label title="A comparison renders at most this many changed records">
          At most
          <input v-model.number="maxDocuments" class="hc-max" type="number" min="1" />
          documents
        </label>
        <button class="hc-btn" type="submit" :disabled="!canCreate">Create comparison</button>
      </template>
      <span v-else class="bad">{{ comparing }}</span>
      <button class="hc-btn muted" type="button" @click="comparing = null">Cancel</button>
    </form>

    <p v-if="busy && lines.length === 0" class="hc-note">Reading the commit log…</p>
    <p v-else-if="error && lines.length === 0" class="hc-note bad">{{ error }}</p>
    <p v-else-if="lines.length === 0" class="hc-note">
      No doltlite store under <code>{{ storeNote }}</code> yet. A step that has never run has
      written nothing, and the QMD index keeps no store of its own.
    </p>
    <div v-else class="hc-grid">
      <!-- Stores open, commits closed until asked. -->
      <TableGrid
        :columns="historyColumns"
        :rows="lines"
        :tree="true"
        :selectable="true"
        :menu="menu"
        :openByDefault="(r: HistoryRow) => r.level === 'store'"
        :columnOverrides="historyOverrides"
      />
    </div>
  </div>
</template>
