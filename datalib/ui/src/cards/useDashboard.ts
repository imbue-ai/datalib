// What the Dashboard reads and does, for the whole Dashboard card and for
// each of its sections shown as a card of its own: the manage rows and
// the newest documents, kept live, and the actions its buttons take.
// Read from endpoints other cards already use (the manage rows and the
// search), so it adds no backend of its own.
import { computed, onBeforeUnmount, onMounted, reactive, ref } from "vue";
import { UNIFIED_INDEX, type ManageResponse, type ManageRow, type SearchRow } from "@/api";
import { useApi } from "@/cards/cardApi";
import { changed, subscribeLive } from "@/live";
import { formatRelative } from "@/config/timeFormat";
import { syncAllButton } from "@/config/syncAll";
import { isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";
import { copyToClipboard } from "@/clipboard";
import { pushToast } from "@/toasts";
import { logSource } from "./libs/logView";
import { statusTone, needsYou, type Tone } from "./dashboard";
import type { CardCtx } from "./types";

// `recent`: whether to load the newest documents, which only the
// activity section shows.
export function useDashboard(ctx: CardCtx, opts: { recent: boolean }) {
  const api = useApi();
  const manage = ref<ManageResponse | null>(null);
  const recent = ref<SearchRow[]>([]);
  const loadError = ref<string | null>(null);
  const now = ref(Date.now());

  async function loadRows() {
    try {
      manage.value = await api.fetchManageRows();
      loadError.value = null;
    } catch (e) {
      loadError.value = (e as Error).message;
    }
  }

  async function loadRecent() {
    if (!opts.recent) return;
    try {
      recent.value = (await api.fetchSearch("is:document", 12)).rows;
    } catch {
      // No index yet, or the applet is starting: the panel says so.
      recent.value = [];
    }
  }

  const groups = computed(() => (manage.value?.rows ?? []).filter((r) => r.kind === "group"));
  const sources = computed(() => groups.value.filter((r) => r.type !== null && !r.dropped));
  const attention = computed(() => needsYou(groups.value));

  const totalItems = computed(() => sources.value.reduce((n, r) => n + (r.items.value ?? 0), 0));
  const rootBytes = computed(() => manage.value?.storage.root.bytes ?? 0);

  /// The storage bar: one segment per source, largest first, then what
  /// the index and the app's own stores take.
  const segments = computed(() => {
    const total = rootBytes.value;
    if (total <= 0) return [];
    const bySource = sources.value
      .map((r) => ({ id: r.id, label: r.name.label, bytes: r.disk.value ?? 0 }))
      .filter((s) => s.bytes > 0)
      .sort((a, b) => b.bytes - a.bytes);
    const shown = bySource.slice(0, 5);
    const rest = total - shown.reduce((n, s) => n + s.bytes, 0);
    const out = shown.map((s, i) => ({ ...s, tone: `seg-${i}` }));
    if (rest > 0)
      out.push({ id: "rest", label: "Search index and app data", bytes: rest, tone: "seg-rest" });
    return out.map((s) => ({ ...s, pct: (100 * s.bytes) / total }));
  });

  /// A library with no sources, or with sources that have never synced,
  /// gets one next step instead of a status: add a source, or start the
  /// first sync.
  const stage = computed<"loading" | "empty" | "first_sync" | "normal">(() => {
    if (!manage.value) return "loading";
    if (sources.value.length === 0) return "empty";
    if (!manage.value.run) return "first_sync";
    return "normal";
  });

  const lastRun = computed(() => {
    const run = manage.value?.run;
    if (stage.value === "empty") return { tone: "muted" as Tone, text: "Nothing to sync yet" };
    if (stage.value === "first_sync") return { tone: "run" as Tone, text: "Start your first sync" };
    if (!run) return { tone: "muted" as Tone, text: "Not synced yet" };
    if (run.live) return { tone: "run" as Tone, text: "Syncing now" };
    return { tone: "ok" as Tone, text: `Synced ${formatRelative(run.finished_at, now.value)}` };
  });

  const syncAll = computed(() => syncAllButton(sources.value));

  async function syncEverything() {
    const b = syncAll.value;
    try {
      if (b.stops.length > 0) for (const id of b.stops) await api.stopRequest(id);
      else await api.openRequest([]);
    } catch (e) {
      pushToast((e as Error).message);
    }
  }

  async function syncRow(row: ManageRow) {
    try {
      await api.openRequest(row.seeds);
    } catch (e) {
      pushToast((e as Error).message);
    }
  }

  function open(...cardSources: string[]) {
    ctx.host.openCards(...cardSources);
  }

  function addSource() {
    open(`sourcesView(${JSON.stringify({ add: true })})`);
  }

  function openLog(row: ManageRow) {
    open(
      logSource({ step: row.status_from ?? row.id, run: row.last_run_id || null, jumpToEnd: true }),
    );
  }

  function openProblems(row: ManageRow) {
    const everything = row.id === "unified_index";
    const q = {
      url: `${UNIFIED_INDEX}/problems`,
      q: everything ? "" : `source_id:${row.id}`,
      name: everything ? "Problems" : `Problems: ${row.name.label}`,
    };
    open(`gridView(${JSON.stringify(q)})`);
  }

  function openDocument(row: SearchRow) {
    if (row.markdown_uuid) open(`documentView(${JSON.stringify(row.markdown_uuid)})`);
  }

  const canReveal = isDesktopApp();
  const revealLabel = revealActionLabel();
  async function showRoot() {
    const abs = manage.value?.storage.root.abs;
    if (!abs) return;
    if (canReveal) {
      await revealInFileManager(abs);
      return;
    }
    const ok = await copyToClipboard(abs);
    pushToast(ok ? "Data root path copied" : "Could not copy the path", ok ? "info" : "error");
  }

  const dateFormat = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
  function when(iso: string | null): string {
    if (!iso) return "";
    const t = Date.parse(iso);
    if (!Number.isFinite(t)) return "";
    return now.value - t < 86_400_000 ? formatRelative(iso, now.value) : dateFormat.format(t);
  }

  function statusClass(row: ManageRow): string {
    return `tone-${statusTone(row.status.key)}`;
  }

  let stop: (() => void) | null = null;
  let tick: ReturnType<typeof setInterval> | null = null;
  onMounted(() => {
    void loadRows();
    void loadRecent();
    stop = subscribeLive({
      root: (e) => {
        if (changed(e, "manage.rows") || changed(e, "storage")) void loadRows();
        if (e.kind === "index_changed") void loadRecent();
      },
      resync: () => {
        void loadRows();
        void loadRecent();
      },
    });
    // "Synced 5 minutes ago" keeps counting while the card sits open.
    tick = setInterval(() => (now.value = Date.now()), 30_000);
  });
  onBeforeUnmount(() => {
    stop?.();
    if (tick) clearInterval(tick);
  });

  // Reactive, so a template reads `d.sources` rather than `d.sources.value`.
  return reactive({
    recent,
    loadError,
    sources,
    attention,
    totalItems,
    rootBytes,
    segments,
    stage,
    lastRun,
    syncAll,
    syncEverything,
    syncRow,
    open,
    addSource,
    openLog,
    openProblems,
    openDocument,
    canReveal,
    revealLabel,
    showRoot,
    when,
    statusClass,
  });
}

export type Dashboard = ReturnType<typeof useDashboard>;
