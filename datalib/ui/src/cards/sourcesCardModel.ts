// The Sources card's logic — the config it edits, the rows it shows,
// the wizard, removal, the menu and the row actions. SourcesCard.ce.vue
// owns only the template and the styles.
import { computed, onMounted, onUnmounted, ref } from "vue";
import { TOPIC_CONFIG_WRITTEN, type CardCtx } from "./types";
import { UNIFIED_INDEX, type ManageResponse, type ManageRow } from "@/api";
import { useApi } from "@/cards/cardApi";
import {
  listGroups,
  listSteps,
  insertEntries,
  moveGroup,
  moveAgainstDataFlow,
  removeSteps,
  describeGroup,
  renameGroup,
  replaceSteps,
  sourceStepsOf,
  removedWith,
  setQmdSteps,
  qmdIndexingOf,
  type QmdIndexing,
  unwireFromFanIns,
  wireIntoFanIns,
  paramsAreRepresentable,
  entryForStep,
  emptyTableDiagnosis,
  DIFF_TYPE,
  type ConfiguredGroup,
  type ConfiguredStep,
  type SourceSteps,
  type StepPhase,
} from "@/config/sourceSteps";
import { syncAllButton } from "@/config/syncAll";
import type { TableGridApi } from "./tableGridApi";
import type { MenuEntry } from "@/grid/menu";
import { catalogForStep, type CatalogEntry } from "@/config/catalog";
import { ingestLabel } from "@/config/ingestMethods";
import { rowMenu, type MenuAction } from "@/config/rowMenu";
import { menuTarget, rowActions, withBrowse, type ActionRow } from "./rowActions";
import { changed, subscribeLive } from "@/live";
import { confirmAction, isDesktopApp, revealActionLabel } from "@/desktop";

export const SOURCES_HELP = `
<p>Every top-level row is a <b>group</b> <code>config.toml</code> declares: a source
(Work Slack, Personal mail), or the unified index that makes them searchable. Open
its chevron for the <b>steps</b> that do the work — fetch, render, index — and the
<b>applets</b> the app spawns to serve it. Actions that don’t apply to a kind are
disabled and say why.</p>
<p>Each row says what its step is doing now. <b>Sync</b> on a source fetches what’s
new, then rebuilds everything downstream: its own steps, and the index every source
feeds. Each of those rows then shows the sync in its own <b>Status</b> — queued,
and what it waits for; running; then how it ended — so pressing Sync on one row moves
others too. Syncs run side by side: a source synced while another syncs starts at
once.</p>
<p>On a step that reads another — a render, the index — Sync reruns it on what its
inputs already hold, then rebuilds everything downstream; nothing upstream runs. It
is offered while that step is out of date: its code, its settings or what it reads
changed since it last succeeded, as after an upgrade that changes how a source
renders.</p>
<p>While a row has work left in a sync, its Sync button is a <b>Stop</b> that names
the sync — “Stop the sync of Work Gmail” — and who started it, if not you. Each source
syncs on its own, even under Sync everything, so a source’s Stop stops that source.
The index every source feeds is part of each of their syncs, and its Stop stops all
of them. The steps in flight checkpoint what they have and exit; until they do the
button reads Stopping. The button at the top syncs everything, and while anything
syncs it stops everything.</p>
<p>The <b>switch</b> at the end of a row says whether it runs in syncs. Turned off,
every sync skips it, and if it is running it stops; what reads it waits. Turned back
on, it runs in the next sync — turning it on starts nothing by itself. On a group it
turns every step under it off or on. Hover it to see who turned it off.</p>
<p>A group row reads off its steps: <b>Status</b> shows the one that matters most —
running if any step is, else waiting, failed, queued, off or stopped if any is, and otherwise
the last step’s in pipeline order. <b>Last synced</b> and
<b>Last success</b> are the fetch step’s. <b>Remove</b> takes the steps and applets
with it.</p>
<p>Rows come in the order <code>config.toml</code> lists them. Click a header to sort by
that column; a third click puts the config's order back. <b>Drag a group</b> by the grip
at its left to move it: the move is written to <code>config.toml</code>, with everything
under the group kept beside it. Groups move only while the table is in the config's
order, and never above a group they read from.</p>
<p><b>Name</b> stays in view while the table scrolls sideways. <b>Status</b> leads
with an icon for what the row is doing or did last, then says when it got there. That
icon, and the mark before a name — the service a source mirrors, or what a step
does — give their word on hover. Hovering a name shows its id: for a group, the
folder its data is in.
<b>Right-click a header</b> to show or hide columns: <b>Last synced</b> and
<b>Last success</b> start hidden. <b>Double-click a Status</b> to read that
step's log — from the run in flight while it runs, else from the run it last took
part in, with a picker for its other runs — as a grid you can sort, filter and
search; on a group row, the log of the step its status came from.
A <b>red or yellow number</b> after a name counts the errors and warnings that step
found in its last run; a source's is the sum of its steps'. A row with none shows
nothing. <b>Double-click the number</b> for the list.
While a row has work queued, its Status goes on to say how much the step says is
still ahead of it (<i>1,204 to go</i>), and when
that work is done (<i>3m left</i>) at the pace work has come off the queue over the last two minutes
(since the step started, when nothing came off in those two) — or a word when there
is no pace to go by: <i>stalled</i> (nothing has moved for a minute),
<i>measuring</i> (nothing has come off yet), <i>growing</i>, <i>flat</i>. A queue a
step reads in passes climbs as work arrives and falls to nothing when a pass ends,
so its ETA holds steady through the climb rather than reading it as growth. A group shows no queue, since its steps
count different things, and waits on its slowest step; a stall anywhere under it shows. Hover either for how it
was reached. The <b>chart button</b> in a row's Actions — or <b>Show sync dashboard</b> on its
menu — opens the group's
<b>sync dashboard</b>: its row and each step's,
laid out one under another with the same actions, charts over the run — what was
queued and done, rows written, requests made, checkpoints, warnings and errors
logged, size on disk — and the group's log.</p>
<p><b>Browse</b>, <b>Sync</b>, the dashboard and the switch are on the row: they are what a row
does often. <b>Right-click a row</b> for everything it can do — sync, edit, rename,
the log, reveal, <b>Reset</b>, remove — and its <b>commit history</b>: every store
under it is versioned, and the history opens beside this card with each commit — when,
what it said, what it did to each table, and the run that made it — newest first,
updating while a sync runs. On a source, <b>Compare two versions</b> opens it ready to
compare the last two syncs.
Right-click inside a selection and the menu acts on all of it; outside one, on that
row alone, without changing the selection. An entry that doesn’t apply stays, greyed,
and says why on hover.</p>
<p><b>Reset</b> empties what a step holds: every row goes, and the history keeps
them, so a wrong click is a revert. Reset a source, or its download, and what reads
it catches up at once, so its documents leave the grid; the next Sync downloads it
all again from nothing. Reset a render and it renders its documents again from what
is downloaded, at once.</p>
<p><b>Items</b> is how many things this source holds: messages in a chat source (not
the tool calls or system notes between them), readings in a sensor source, a pull
request and its comments, one per event, contact, page or PDF, one per file or photo in
a library. It is counted over the whole store, not this run, by the render step: on its
own row and on the group above it. It moves while a render runs, each time the step
seals what it has written, and its line covers the last few days of syncs. Hover for
how many documents — what <b>Browse</b> opens — the items sit in. A blank cell means
nothing has counted yet.</p>
<p><b>Size</b> is bytes on disk, from a directory walk over each row’s tree — a group’s
is its whole folder, measured on the same walk — plotted over the last few minutes, with
its change over that time beside it. Each row’s line is scaled to its own range, so a
jump in a small source shows as plainly as one in a large one: the line is the shape of
the change, and the numbers are its size. Hover for the breakdown.</p>
<p><b>Status</b> and <b>Last synced</b> are per step, read from the runner’s own
record — so a sync you or an agent start from a terminal shows up here too.
<b>Last success</b> is when the step last ran without failing: when it is older than
Last synced, every run since has failed, and a source's mirror is only known to match
upstream as of then. A step whose sync ended before it finished — the app was quit
mid-run, say — reads as <b>interrupted</b>.</p>
<p>The bar along the bottom of the app is the <b>whole data root</b>, not the sum of
the rows: it includes <code>system/</code> — the stores, the run log, the served
attachments — and anything a deleted step left behind. The config itself is the
<b>config.toml</b> card; <b>Show the config</b> opens it beside this one.</p>
`;

/// What a row stands for: a `[[groups]]` entry, or one of the two
/// kinds of entry filed under it. The server assembles the row
/// (`GET /api/manage/rows`), typed by the columns it declares; what is
/// added here is what needs the wizard's descriptors, which live in
/// the browser.
export type Row = ActionRow & {
  /// Null when the wizard can edit this row; otherwise why not.
  editBlocked: string | null;
  /// The group whose form Edit opens: the row's own group, or for a
  /// step under one, that group. Null where there is no form.
  editGroup: string | null;
};

export function useSourcesCard(ctx: CardCtx) {
  const api = useApi();
  const {
    fetchConfig,
    fetchConfigScaffold,
    saveConfig,
    fetchManageRows,
    fetchRequests,
    purgeGroups,
  } = api;

  /// The config editor, as a card beside this one.
  function openConfig() {
    ctx.host.openCards("configView()");
  }

  const configText = ref("");
  const configPath = ref("");
  // Two independent verdicts on the config, and both matter.
  const parseError = ref<string | null>(null);
  const configError = ref<string | null>(null);
  // What the backend's own loader made of the same file. Held so the
  // empty state can cross-check itself against it — see
  // `emptyTableDiagnosis`.
  const serverSourceCount = ref(0);
  const configExists = ref(false);
  const loadError = ref<string | null>(null);
  const banner = ref<{ ok: boolean; text: string } | null>(null);
  // The requests a banner is about, when it is about some. Such a banner
  // retires once they have all closed, not on the next action.
  const bannerRequests = ref<string[]>([]);

  /// Put up a banner, optionally tying it to requests' lifetimes.
  function say(ok: boolean, text: string, requestIds: string[] = []) {
    banner.value = { ok, text };
    bannerRequests.value = requestIds;
  }

  function clearBanner() {
    banner.value = null;
    bannerRequests.value = [];
  }

  /// Take down a request's banner once its requests have closed.
  async function retireBanner() {
    const ids = bannerRequests.value;
    if (ids.length === 0) return;
    try {
      const open = (await fetchRequests()).some((r) => ids.includes(r.id) && r.state === "open");
      if (!open && bannerRequests.value === ids) clearBanner();
    } catch {
      // The banner stays until the next action.
    }
  }
  const busy = ref(false);
  /// The rows, joined server-side from the config, the loop's record and
  /// requests, the run store and the usage sampler — see
  /// `datalib/backend/http/src/manage/`. Bytes are measured by the backend
  /// on a tick *while a sync is running*, not walked per request; between
  /// runs nothing walks, which is why the two loads that matter ask for a
  /// fresh one.
  const manage = ref<ManageResponse | null>(null);
  const storage = computed(() => manage.value?.storage ?? null);
  /// The config's entries as the browser parses them, for the wizard —
  /// which edits the text — and the catalog lookups in `decorate`.
  const sources = ref<ConfiguredStep[]>([]);
  /// The `[[groups]]` entries, for what a new source may not collide with
  /// and for taking a group with its last step.
  const configGroups = ref<ConfiguredGroup[]>([]);

  // Resolved once — the desktop bridge either exists for this window or
  // it doesn't, and the label depends only on the platform.
  const canReveal = isDesktopApp();
  const revealLabel = revealActionLabel();

  const wizardOpen = ref(false);
  /// Bumped on every opening, and bound to the dialog's `key`, so a
  /// reopened dialog is a fresh mount rather than a reused component
  /// still holding the last one's refs.
  const wizardKey = ref(0);
  /// The source the wizard is editing: its group, the catalog entry that
  /// describes it, and whichever of its two steps the config has.
  const editing = ref<{
    group: ConfiguredGroup;
    entry: CatalogEntry;
    steps: SourceSteps;
    qmdIndexing: QmdIndexing;
  } | null>(null);

  /// Non-null when the table is empty for a reason worth shouting about
  /// rather than the ordinary "you haven't added anything yet".
  const emptyDiagnosis = computed(() =>
    emptyTableDiagnosis({
      parsedCount: sources.value.length,
      serverSourceCount: serverSourceCount.value,
      textLength: configText.value.length,
      exists: configExists.value,
      path: configPath.value,
    }),
  );

  /// Ids already spoken for: every group, plus the written id of every
  /// step outside a group. A custom step's id is reserved whole, not by
  /// its first path segment: the loader allows a group `exports` beside a
  /// custom `exports/csv` (their trees differ), and nothing here splits an
  /// id — the cost is that the group's measured folder then counts the
  /// custom tree too. An applet id lives in another namespace and may
  /// coincide.
  const takenIds = computed(
    () =>
      new Set([
        ...configGroups.value.map((g) => g.id),
        ...sources.value.filter((s) => s.kind === "step").map((s) => s.group ?? s.id),
      ]),
  );

  /// The render step that reads a given fetch step, if the config has
  /// one — what deleting the fetch step has to take with it.
  function renderSiblingOf(fetchId: string): ConfiguredStep | undefined {
    return sources.value.find(
      (s) => s.kind === "step" && s.inputs.includes(fetchId) && s.phase === "render",
    );
  }

  /// The tree the grid shows, as the server assembled it, with the
  /// wizard's knowledge added per row.
  const rows = computed<Row[]>(() => {
    const all = manage.value?.rows ?? [];
    const groups = new Map(all.filter((r) => r.kind === "group").map((g) => [g.id, g]));
    return all.map((r) => decorate(r, groups));
  });

  const syncAll = computed(() => syncAllButton(rows.value));

  function decorate(r: ManageRow, groups: Map<string, ManageRow>): Row {
    const browsing = withBrowse(r, groups, canReveal);
    // `system/` is not a config entry: nothing to edit.
    if (r.kind === "system") {
      return { ...browsing, editBlocked: "Not a config entry.", editGroup: null };
    }
    if (r.kind === "group") {
      const editBlocked = groupEditBlocked(r.id);
      return { ...browsing, editBlocked, editGroup: editBlocked ? null : r.id };
    }
    // Edit: the wizard's one form describes a source — a group and its two
    // steps — so a step under a group edits through its group. Everything
    // else is hand-written config, and the honest answer is to say so.
    // Deliberately no dropped-entry override: editing is how the entry
    // gets fixed.
    let editBlocked: string | null;
    if (r.kind === "applet") {
      editBlocked = "No form for applets — edit this one in Advanced below.";
    } else if (r.phase === "index") {
      editBlocked = "A shared index step has no options — its inputs are its whole config.";
    } else if (r.written_group !== null) {
      // The written group, not the declared one: a step naming a group
      // the config lacks should hear that, not "outside any group".
      editBlocked = groupEditBlocked(r.written_group);
    } else {
      editBlocked = "No guided form for a step outside a group — edit it in Advanced below.";
    }
    // "Download" or "Import", read off the step's params against what its
    // provider declares; the server's "Ingest" only when they name no method.
    const ingestLabelled =
      r.group && r.phase === "ingest" ? ingestLabel(r.type?.id ?? null, r.params) : null;
    return {
      ...browsing,
      name: ingestLabelled ? { ...r.name, label: ingestLabelled } : r.name,
      editBlocked,
      editGroup: editBlocked ? null : r.group,
    };
  }

  /// Why a group has no form, or null when the wizard can edit it. A
  /// source is edited as one thing, so the verdict is the group's and
  /// every step under it shows the same one.
  function groupEditBlocked(groupId: string): string | null {
    const g = configGroups.value.find((x) => x.id === groupId);
    if (!g) return "This step names a group the config doesn't declare.";
    const { ingest, render } = sourceStepsOf(g.id, sources.value);
    const entry = groupEntry(g, { ingest, render });
    if (!g.type) return "No guided form for this group — edit its entries in Advanced below.";
    if (g.type === "diff") {
      return (
        "A diff group compares two commits of its source; change them under " +
        "`params.diff` in Advanced below, or remove the group."
      );
    }
    if (!entry) return `No guided form: the catalog doesn't know the type "${g.type}".`;
    if (!entry.wizard) return `No guided form for ${entry.label} yet — edit it in Advanced below.`;
    for (const step of [ingest, render]) {
      if (!step) continue;
      const rep = paramsAreRepresentable(step, entry);
      if (!rep.ok) {
        return (
          `The form doesn't model ${rep.unknown.join(", ")} on ${step.id}, and saving would ` +
          `drop it. Edit this one in Advanced below.`
        );
      }
    }
    return null;
  }

  /// The catalog entry describing a group. Its `type` names the provider,
  /// but *which* descriptor — Gmail or Fastmail, both `email` — is read
  /// off its ingest step's params, the way the step row does it.
  function groupEntry(g: ConfiguredGroup, steps: SourceSteps): CatalogEntry | undefined {
    const step = steps.ingest ?? steps.render;
    return step ? entryForStep(step, sources.value) : catalogForStep(g.type, {});
  }

  /// The actions every Manage row offers, shared with the sync dashboard.
  /// A step's banner name is the one the config gives it.
  const actions = rowActions<Row>({
    api,
    host: ctx.host,
    rows: () => rows.value,
    run: () => manage.value?.run ?? null,
    say,
    clear: clearBanner,
    busy,
    reload: (fresh) => loadRows(fresh),
    shownName: (row) =>
      row.kind === "group"
        ? row.name.label
        : (sources.value.find((s) => s.id === row.id)?.name ?? row.id),
  });

  let gridApi: TableGridApi<Row> | null = null;
  function onGridReady(api: TableGridApi<Row>) {
    gridApi = api;
  }

  /// Commit only the newest answer, whatever order the answers arrive in.
  ///
  /// Not theoretical: rows fetched before a sync was asked for but landing after
  /// the ones fetched once it was paint the row backwards.
  /// `data-sources-sync`'s monotonicity test catches it.
  function freshest<T>(commit: (value: T) => void) {
    let issued = 0;
    let committed = 0;
    const run = async (load: () => Promise<T>) => {
      const seq = ++issued;
      const value = await load();
      if (seq <= committed) return;
      committed = seq;
      commit(value);
    };
    /// Drop everything already in flight.
    run.invalidate = () => {
      committed = issued;
    };
    return run as typeof run & { invalidate: () => void };
  }

  /// Double-clicking a status opens the log it came from; a problems count,
  /// the problems, or on System, whose count is the config's warnings, the
  /// config.
  function onCellDoubleClicked(data: Row, field: string) {
    if (field === "problems" && data.kind === "system") openConfig();
    else if (field === "problems") actions.openProblems(data, `${UNIFIED_INDEX}/problems`);
    else if (field === "status") void actions.openStepLog(data);
  }

  /// An in-place edit of the Name cell: a group's rename.
  function onCellEdit(row: Row, field: string, value: string) {
    if (field === "name" && row.kind === "group") void renameRow(row, value);
  }

  // ── The right-click menu. Every action a row offers, in one place,
  // with Lightroom semantics: right-click a row inside the selection and
  // the whole selection is the target; outside it, that row alone, and
  // the selection stays as it was. An entry that does not apply stays,
  // disabled, with the reason as its tooltip — see `config/rowMenu.ts`.

  function contextMenuItems(anchor: Row, targets: Row[]): MenuEntry[] {
    if (targets.length === 0) return [];
    const target = (t: Row) => menuTarget(t, t.editBlocked);
    return rowMenu(targets.map(target), { canReveal, revealLabel }).map((entry) =>
      entry.separator
        ? { name: "", separator: true }
        : {
            name: entry.name,
            disabled: entry.disabled,
            danger: ["remove", "reset"].includes(entry.action),
            action: () => void runMenuAction(entry.action, targets, anchor),
          },
    );
  }

  /// What only this card can do — it holds the config's text and the
  /// wizard — and otherwise what every row card does.
  async function runMenuAction(action: MenuAction, targets: Row[], anchor: Row) {
    const [first] = targets;
    switch (action) {
      case "edit":
        if (first.editGroup) await openEdit(first.editGroup);
        return;
      case "compare":
        actions.openHistory([first], true);
        return;
      case "rename":
        gridApi?.startEditing(anchor, "name");
        return;
      case "remove":
        await deleteRows(targets);
        return;
      default:
        await actions.runMenuAction(action, targets);
    }
  }

  /// Write a group's new name, or drop the line when it is blank or is
  /// the id again — `renameGroup` treats both as "no name".
  async function renameRow(row: Row, name: string) {
    if (row.kind !== "group") return;
    const next = renameGroup(configText.value, row.id, name);
    if (next === configText.value) return;
    await writeConfig(
      next,
      name ? `Renamed ${row.id} to ${name}.` : `Cleared the name of ${row.id}.`,
    );
  }

  /// Groups are dragged into a new order; nothing else is, and nothing
  /// while the config cannot be written.
  function isMovable(row: Row): boolean {
    return row.kind === "group" && !busy.value && !parseError.value && !configError.value;
  }

  /// Write a dragged group down where it was dropped. Read off the config
  /// as the server has it, as `openEdit` does, so a move cannot write back
  /// over an editor save that has not reached this card yet.
  async function onRowMove(row: Row, before: Row | null) {
    if (row.kind !== "group") return;
    await loadConfig();
    const name = (id: string) =>
      rows.value.find((r) => r.kind === "group" && r.id === id)?.name.label ?? id;
    const against = moveAgainstDataFlow(configText.value, row.id, before?.id ?? null);
    if (against) {
      say(
        false,
        "reads" in against
          ? `${row.name.label} can't go above ${name(against.reads)}: it reads what ${name(against.reads)} makes, and the config lists each group below the ones it reads.`
          : `${row.name.label} can't go below ${name(against.readBy)}: ${name(against.readBy)} reads what it makes, and the config lists each group below the ones it reads.`,
      );
      return;
    }
    let next: string;
    try {
      next = moveGroup(configText.value, row.id, before?.id ?? null);
    } catch (e) {
      say(false, (e as Error).message);
      return;
    }
    if (next === configText.value) return;
    const where = before ? `above ${before.name.label}` : "to the bottom";
    await writeConfig(next, `Moved ${row.name.label} ${where}.`);
  }

  // ── Which groups are open. Remembered per browser, so a reload — or
  // the remount a sync's end does — puts the table back the way it was.
  // A convenience, not state: nothing breaks when it is empty.
  const EXPANDED_STORE = "datalib.manage.expanded";

  function readExpanded(): Set<string> {
    try {
      const raw = localStorage.getItem(EXPANDED_STORE);
      const list: unknown = raw ? JSON.parse(raw) : [];
      return new Set(
        Array.isArray(list) ? list.filter((x): x is string => typeof x === "string") : [],
      );
    } catch {
      return new Set();
    }
  }
  const expandedGroups = readExpanded();

  function isGroupOpenByDefault(row: Row): boolean {
    return expandedGroups.has(row.key);
  }

  function onRowGroupOpened(row: Row, expanded: boolean) {
    const key = row.key;
    if (!key) return;
    if (expanded) expandedGroups.add(key);
    else expandedGroups.delete(key);
    try {
      localStorage.setItem(EXPANDED_STORE, JSON.stringify([...expandedGroups]));
    } catch {
      // Storage refused — private mode, quota — and the chevron still
      // works; only the memory across reloads is lost.
    }
  }

  function reparse() {
    try {
      sources.value = listSteps(configText.value);
      configGroups.value = listGroups(configText.value);
      parseError.value = null;
    } catch (e) {
      parseError.value = (e as Error).message;
    }
  }

  async function loadConfig() {
    // Cleared on success, not before the fetch: a banner that goes and
    // comes back on every reload moves the table twice.
    try {
      let cfg = await fetchConfig();
      if (!cfg.exists) cfg = await fetchConfigScaffold();
      configPath.value = cfg.path;
      // `parsed_ok` false means the file is not a config at all — in
      // which case `App.vue`'s gate is showing instead of this view, and
      // this is belt and braces. Ordinary per-entry problems are not
      // errors of the whole config and live in `configDiagnostics`.
      configError.value = cfg.parsed_ok ? null : (cfg.error ?? "The config was rejected.");
      serverSourceCount.value = cfg.source_count;
      configExists.value = cfg.exists;
      configText.value = cfg.text;
      loadError.value = null;
      reparse();
      if (sources.value.length === 0 && cfg.source_count > 0) {
        // The inspector is the only channel when someone hits this in the
        // desktop app and can't copy text out of a banner.
        console.warn(
          "sources card: parsed 0 entries from a config the server reads",
          cfg.source_count,
          "sources from —",
          { path: cfg.path, textLength: cfg.text.length, parsedOk: cfg.parsed_ok },
        );
      }
    } catch (e) {
      loadError.value = (e as Error).message;
    }
  }

  /// The rows the loader dropped, for the banner above the table.
  const droppedRows = computed(() => rows.value.filter((r) => r.dropped));

  const commitRows = freshest<ManageResponse>((m) => {
    manage.value = m;
  });

  /// Read the rows. `refresh` asks the backend to walk the disk before
  /// answering rather than serving its last tick — see `fetchManageRows`.
  async function loadRows(refresh = false) {
    try {
      await commitRows(() => fetchManageRows(refresh));
    } catch {
      // The last answer stands; an empty table over an error banner would
      // read as "no sources".
    }
  }

  /// Write new config text and adopt whatever the backend then reports.
  /// The backend validates with the real loader — including the duplicate
  /// and reserved-name checks — so a rejection comes back as `ok:false`
  /// with the loader's message rather than a thrown error.
  async function writeConfig(text: string, what: string) {
    busy.value = true;
    clearBanner();
    try {
      const res = await saveConfig(text);
      if (!res.ok) {
        banner.value = { ok: false, text: res.error ?? "The config was rejected." };
        return false;
      }
      configText.value = text;
      reparse();
      ctx.bus.publish(TOPIC_CONFIG_WRITTEN, null);
      // A warning saves — nothing is dropped — but it is still advice
      // the file would otherwise only give on the command line.
      banner.value = { ok: true, text: res.error ? `${what} Warning: ${res.error}` : what };
      return true;
    } catch (e) {
      banner.value = { ok: false, text: (e as Error).message };
      return false;
    } finally {
      busy.value = false;
    }
  }

  function closeWizard() {
    wizardOpen.value = false;
    editing.value = null;
  }

  function openAdd() {
    editing.value = null;
    wizardKey.value++;
    wizardOpen.value = true;
  }

  /// Open the wizard on a source: its group, with both its steps' values
  /// in one form.
  /// The form reads the config as the server has it: a save from the
  /// editor reaches this card by a pushed frame, and an Edit clicked before
  /// that lands would open on the text from before it.
  async function openEdit(groupId: string) {
    await loadConfig();
    const group = configGroups.value.find((g) => g.id === groupId);
    if (!group) return;
    const steps = sourceStepsOf(group.id, sources.value);
    const entry = groupEntry(group, steps);
    if (!entry) return;
    const qmdIndexing = steps.render ? qmdIndexingOf(sources.value, group.id) : "keyword_and_embed";
    editing.value = { group, entry, steps, qmdIndexing };
    wizardKey.value++;
    wizardOpen.value = true;
  }

  async function onWizardSubmit(payload: {
    id: string;
    name: string;
    description: string;
    entry: CatalogEntry;
    groupBody: string | null;
    stepsBody: string;
    renderId: string | null;
    qmdIndexing: QmdIndexing;
  }) {
    const current = editing.value;
    let next: string;
    if (current) {
      // Both steps are replaced in one cut-and-append, and a step the
      // source was missing is simply appended with the other. The name
      // and the description live on the group, which is edited in place.
      const existing = [current.steps.ingest, current.steps.render].filter(
        (s): s is ConfiguredStep => !!s,
      );
      next = replaceSteps(configText.value, existing, payload.stepsBody);
      next = renameGroup(next, current.group.id, payload.name);
      next = describeGroup(next, current.group.id, payload.description);
      // A render step the provider does not write back — hand-written
      // under a download-only type — leaves with the cut above, so its
      // edges have to go too, or the fan-ins name a step that no longer
      // exists and the loader refuses the whole file.
      if (current.steps.render && !payload.renderId) {
        next = unwireFromFanIns(next, current.steps.render.id);
      }
    } else {
      next = insertEntries(
        configText.value,
        payload.groupBody ? `${payload.groupBody}\n\n${payload.stepsBody}` : payload.stepsBody,
      );
    }

    // The fan-ins name their inputs, so a render step added without this
    // renders happily and is never indexed. Idempotent, so re-saving an
    // edit doesn't duplicate the entry. Free-text search is the one index
    // the wizard asks about: the source's own qmd steps, added or taken out.
    if (payload.renderId) {
      next = wireIntoFanIns(next, payload.renderId);
      next = setQmdSteps(next, payload.id, payload.qmdIndexing);
    } else {
      next = setQmdSteps(next, payload.id, "none");
    }

    // Banners are for a person, so they say the name; the id is what the
    // config and the disk use.
    const shown = payload.name || payload.id;
    const ok = await writeConfig(next, current ? `Saved ${shown}.` : `Added ${shown}.`);
    if (!ok) return;
    closeWizard();
  }

  // ── Removing. A comparison's tree is computed from two commits its
  // source keeps, so unlike a download it is worth nothing kept: the
  // question offers to delete it, checked by default. Anything else keeps
  // its data, and gets the platform's plain confirm.
  const asking = ref<{
    message: string;
    checkLabel: string;
    resolve: (answer: { ok: boolean; checked: boolean }) => void;
  } | null>(null);

  function comparisonQuestion(name: string): string {
    return `Remove the comparison "${name}" from the config?`;
  }

  /// The groups whose trees go with the removal, or null for Cancel.
  async function confirmRemoval(
    what: string,
    leaving: ConfiguredGroup[],
  ): Promise<string[] | null> {
    const comparisons = leaving.filter((g) => g.type === DIFF_TYPE);
    if (comparisons.length === 0) return (await confirmAction(what)) ? [] : null;
    const names = comparisons.map((g) => `"${g.name ?? g.id}"`).join(", ");
    const answer = await new Promise<{ ok: boolean; checked: boolean }>((resolve) => {
      asking.value = {
        message: what,
        checkLabel:
          `Also delete the computed changes of ${names} from disk. ` +
          `Comparing the same two syncs again rebuilds them.`,
        resolve,
      };
    });
    asking.value = null;
    if (!answer.ok) return null;
    return answer.checked ? comparisons.map((g) => g.id) : [];
  }

  async function removeAndPurge(next: string, what: string, purge: string[]) {
    const ok = await writeConfig(next, what);
    if (!ok || purge.length === 0) return;
    try {
      const done = await purgeGroups(purge);
      banner.value = {
        ok: true,
        text:
          done === "done"
            ? `${what} The computed changes are deleted.`
            : `${what} The computed changes are deleted once the sync in progress is over.`,
      };
    } catch (e) {
      banner.value = {
        ok: false,
        text: `${what} The computed changes were not deleted: ${(e as Error).message}`,
      };
    }
  }

  async function deleteSource(id: string) {
    const step = sources.value.find((s) => s.id === id);
    if (!step) return;
    const name = step.name;

    // Deleting a fetch step takes its render step too. Leaving the render
    // step behind would leave an input naming a step that no longer
    // exists, which the loader refuses outright — a whole config broken
    // by a partial delete.
    const sibling = step.phase === "ingest" ? renderSiblingOf(step.id) : undefined;
    const readers = removedWith([step.id], sources.value).filter((r) => r.id !== sibling?.id);
    const doomed = [step, ...(sibling ? [sibling] : []), ...readers];
    const alsoGone = readers.length
      ? `\n\nThese go with it: ${readers.map((r) => `"${r.name}"`).join(", ")}.`
      : "";

    // A group with nothing left under it goes too: the loader would only
    // warn about it, but a `[[groups]]` entry naming a source that is
    // gone is litter someone has to explain.
    const goneIds = new Set(doomed.map((d) => d.id));
    const emptied = configGroups.value.filter(
      (g) =>
        step.group === g.id && !sources.value.some((s) => s.group === g.id && !goneIds.has(s.id)),
    );

    const what = emptied.some((g) => g.type === DIFF_TYPE)
      ? comparisonQuestion(name)
      : step.kind === "applet"
        ? `Remove the "${name}" applet from the config?\n\n` +
          `The server stops it. Anything in the app that its components or endpoints ` +
          `serve will stop working until you add it back.`
        : step.phase === "index"
          ? `Remove the "${name}" index step from the config?\n\n` +
            `Its output stays on disk but stops being refreshed, so search results go stale.` +
            alsoGone
          : sibling
            ? `Remove "${name}" and the render step that reads it ("${sibling.name}")?\n\n` +
              `Both have to go together: a render step whose input is gone is a config ` +
              `datalib refuses to load.${alsoGone}\n\n` +
              `The data stays on disk. Re-adding later resumes from what's already there.`
            : `Remove "${name}" from the config?${alsoGone}\n\n` +
              `Its data stays on disk — only this step stops running. Re-adding it later ` +
              `resumes from what's already there.`;
    const purge = await confirmRemoval(what, emptied);
    if (!purge) return;

    // Cut first: the entries' offsets are into the text as parsed, and
    // unwiring a fan-in above the source would shift them. Unwiring is a
    // regex over the result, so it needs no offsets.
    let next = removeSteps(configText.value, [...doomed, ...emptied]);
    for (const d of doomed) next = unwireFromFanIns(next, d.id);
    await removeAndPurge(next, `Removed ${name}.`, purge);
  }

  /// Remove a group with everything filed under it. Its render steps
  /// leave the fan-ins too, or the config would name inputs that no
  /// longer exist and the loader would refuse the whole file.
  async function deleteGroup(id: string) {
    const group = configGroups.value.find((g) => g.id === id);
    if (!group) return;
    const name = group.name ?? group.id;
    const inGroup = sources.value.filter((s) => s.group === id);
    const members = [
      ...inGroup,
      ...removedWith(
        inGroup.map((m) => m.id),
        sources.value,
      ),
    ];
    const steps = members.filter((s) => s.kind === "step").length;
    const applets = members.filter((s) => s.kind === "applet").length;
    const count = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
    const under = [steps ? count(steps, "step") : "", applets ? count(applets, "applet") : ""]
      .filter(Boolean)
      .join(" and ");
    const what =
      group.type === DIFF_TYPE
        ? comparisonQuestion(name)
        : `Remove "${name}" from the config${under ? `, with the ${under} under it` : ""}?\n\n` +
          `The data stays on disk — these entries just stop running. Adding the source ` +
          `back later resumes from what's already there.`;
    const purge = await confirmRemoval(what, [group]);
    if (!purge) return;

    let next = removeSteps(configText.value, [...members, group]);
    for (const m of members) next = unwireFromFanIns(next, m.id);
    await removeAndPurge(next, `Removed ${name}.`, purge);
  }

  /// Several rows at once: one question, one write. A single row keeps
  /// its own wording, which says what else goes with it.
  async function deleteRows(targets: Row[]) {
    if (targets.length === 1) {
      const [row] = targets;
      if (row.kind === "group") await deleteGroup(row.id);
      else await deleteSource(row.id);
      return;
    }
    const doomed = new Map<string, ConfiguredStep | ConfiguredGroup>();
    const groups = new Set<string>();
    for (const t of targets) {
      if (t.kind === "group") {
        const group = configGroups.value.find((g) => g.id === t.id);
        if (!group) continue;
        groups.add(group.id);
        doomed.set(`group:${group.id}`, group);
        for (const m of sources.value.filter((s) => s.group === group.id)) doomed.set(m.id, m);
      } else {
        const step = sources.value.find((s) => s.id === t.id);
        if (!step) continue;
        doomed.set(step.id, step);
        const sibling = step.phase === "ingest" ? renderSiblingOf(step.id) : undefined;
        if (sibling) doomed.set(sibling.id, sibling);
      }
    }
    for (const r of removedWith([...doomed.keys()], sources.value)) doomed.set(r.id, r);
    // A group with nothing left under it goes too, as in `deleteSource`.
    for (const g of configGroups.value) {
      if (groups.has(g.id)) continue;
      const left = sources.value.some((s) => s.group === g.id && !doomed.has(s.id));
      const had = sources.value.some((s) => s.group === g.id);
      if (had && !left) doomed.set(`group:${g.id}`, g);
    }
    const names = targets.map((t) => `"${t.name}"`).join(", ");
    const what =
      `Remove ${names} from the config, with everything under them?\n\n` +
      `The data stays on disk — these entries just stop running. Adding a source ` +
      `back later resumes from what's already there.`;
    const entries = [...doomed.values()];
    const leaving = [...doomed.entries()]
      .filter(([key]) => key.startsWith("group:"))
      .map(([, g]) => g as ConfiguredGroup);
    const purge = await confirmRemoval(what, leaving);
    if (!purge) return;
    let next = removeSteps(configText.value, entries);
    for (const d of entries) {
      if ("phase" in d) next = unwireFromFanIns(next, d.id);
    }
    await removeAndPurge(next, `Removed ${targets.length} entries.`, purge);
  }

  let unsubscribe: (() => void) | null = null;
  const cardEl = ref<HTMLElement | null>(null);

  /// Everything this table shows, refetched together. The rows come from
  /// the server with the config and the loop's record already joined.
  async function reloadAll(freshStorage = false) {
    await Promise.all([loadConfig(), loadRows(freshStorage)]);
  }

  onMounted(async () => {
    // Fresh sizes on the first paint. The backend only walks the disk
    // while a run is in flight, so on an idle root — the usual state —
    // this is the walk that produces the numbers on screen.
    await reloadAll(true);

    unsubscribe = subscribeLive(
      {
        root: (e) => {
          if (changed(e, "manage.rows")) {
            // Deliberately *not* a fresh walk: this fires once a second while
            // a run is going. The sampler is already walking on its own
            // cadence; this just reads what it found.
            void loadRows();
          }
          // The loop's record moving is the nearest thing to "a step
          // committed" — nothing watches the stores themselves — and its
          // requests live beside it.
          if (changed(e, "dag")) void retireBanner();
          if (e.kind === "config_changed") {
            // Config and record together, for the "Never run" reason above.
            void reloadAll();
          }
        },
        // A reconnect means we may have slept through a whole run, and the
        // sampler's own last walk with it. Ask for a fresh one.
        resync: () => void reloadAll(true),
      },
      { onScreen: cardEl.value ?? undefined },
    );
  });

  onUnmounted(() => {
    unsubscribe?.();
    unsubscribe = null;
    gridApi = null;
  });

  return {
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
    storage,
    rows,
    syncAll,
    actions,
    canReveal,
    revealLabel,
    openConfig,
    openAdd,
    contextMenuItems,
    isGroupOpenByDefault,
    onGridReady,
    onCellDoubleClicked,
    onCellEdit,
    onRowGroupOpened,
    isMovable,
    onRowMove,
    wizardOpen,
    wizardKey,
    takenIds,
    editing,
    closeWizard,
    onWizardSubmit,
    asking,
  };
}
