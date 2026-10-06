// What a Manage row can be asked to do, for every card that shows one:
// the Sources table, and the sync dashboard that lays one group's rows
// out vertically. The row says which actions it offers and why one is
// off (`GET /api/manage/rows`, `config/rowMenu.ts`); this is the code
// behind each id. What needs the config's text — edit, rename, remove,
// compare — stays with the Sources card, which holds that text.
import type { Ref } from "vue";
import type { Action, DagRun, ManageRow } from "@/api";
import { browseColumns, browseName, browseQuery } from "@/config/browsePresets";
import {
  RAW_STORE_BROWSE_LABEL,
  notComparableReason,
  type MenuAction,
  type MenuTarget,
} from "@/config/rowMenu";
import { copyToClipboard } from "@/clipboard";
import { confirmAction, openRawStore, revealInFileManager } from "@/desktop";
import { pushToast } from "@/toasts";
import type { Api } from "./cardApi";
import { historySource } from "./libs/historyView";
import { logSource } from "./libs/logView";
import { syncDashboardSource } from "./libs/syncDashboardView";
import type { HostCommands } from "./types";

/// A Manage row with what its Browse opens: a card, or a raw store in
/// another program.
export type ActionRow = ManageRow & {
  /// The card source a Browse of this row opens, or null where the
  /// row's `browse` action says there is nothing to browse.
  browseSource: string | null;
  /// The raw store Browse opens instead of a card: a download step's,
  /// in the desktop app, which is the only host that can open one.
  rawStore: string | null;
};

/// Browse on a download step with a raw store: enabled whatever the
/// group's Browse says, since a source that renders nothing still has
/// the tables it downloaded.
const RAW_STORE_BROWSE: Action = {
  id: "browse",
  label: RAW_STORE_BROWSE_LABEL,
  enabled: true,
  hint:
    "Open what this step downloaded, read-only: in DB Browser for SQLite when that " +
    "opens .doltlite_db files here, otherwise in a doltlite shell.",
  disabled_reason: null,
};

export const browseAction = (r: ManageRow) => r.actions.find((a) => a.id === "browse");

/// What a Browse of this group opens. Whether it can — the group has a
/// render step, and is in the pipeline — is the server's word, carried
/// by the row's `browse` action; this is only the card behind it. The
/// index group has no type: browsing it is the unified projection
/// across every source, which is the card the app already opens on.
function groupBrowse(g: ManageRow): string | null {
  if (!browseAction(g)?.enabled) return null;
  const type = g.type?.id ?? null;
  if (!type) return "gridView()";
  const columns = browseColumns(type);
  const args: string[] = [`q: ${JSON.stringify(browseQuery(g.id, type))}`];
  if (columns) args.push(`columns: ${JSON.stringify(columns)}`);
  args.push(`name: ${JSON.stringify(browseName(g.name.label, type))}`);
  return `gridView({ ${args.join(", ")} })`;
}

/// A row's Browse: `system/` browses the run log, a group its view, a
/// step its group's — or, for a download step in the app, its raw store.
export function withBrowse<R extends ManageRow>(
  r: R,
  groups: Map<string, ManageRow>,
  canReveal: boolean,
): R & Pick<ActionRow, "browseSource" | "rawStore"> {
  if (r.kind === "system") {
    return { ...r, browseSource: browseAction(r)?.enabled ? "logView()" : null, rawStore: null };
  }
  if (r.kind === "group") return { ...r, browseSource: groupBrowse(r), rawStore: null };
  const group = r.group ? groups.get(r.group) : undefined;
  const rawStore = canReveal ? r.raw_store_path : null;
  return {
    ...r,
    actions: rawStore
      ? r.actions.map((a) => (a.id === "browse" ? RAW_STORE_BROWSE : a))
      : r.actions,
    browseSource: r.kind === "step" && group ? groupBrowse(group) : null,
    rawStore,
  };
}

/// The slice of a row the menu reads. `editBlocked` is the caller's:
/// only a card holding the config can say whether the form edits it.
export function menuTarget(row: ActionRow, editBlocked: string | null): MenuTarget {
  return {
    id: row.id,
    name: row.name.label,
    kind: row.kind,
    type: row.type?.id ?? null,
    func: row.function,
    runBlocked: row.actions.find((a) => a.id === "sync")?.disabled_reason ?? null,
    editBlocked,
    revealBlocked: row.reveal_blocked,
    browseBlocked: browseAction(row)?.disabled_reason ?? null,
    rawStore: row.rawStore !== null,
    stopRequestIds: row.stop_request_ids,
    turnedOffBy: row.turned_off_by,
    statusFrom: row.status_from,
    dashboardGroup: row.kind === "group" ? row.id : row.kind === "step" ? row.group : null,
    revealPath: row.reveal_path,
  };
}

export type RowActionHost<R extends ActionRow> = {
  api: Api;
  host: HostCommands;
  /// Every row the card holds, for a group's steps and its status child.
  rows: () => R[];
  /// The run in flight, or the one that finished last.
  run: () => DagRun | null;
  /// Put up a message, tied to requests' lifetimes when it names some.
  say: (ok: boolean, text: string, requestIds?: string[]) => void;
  clear: () => void;
  busy: Ref<boolean>;
  /// Read the rows again; `freshStorage` walks the disk first.
  reload: (freshStorage?: boolean) => Promise<void>;
  /// What a banner calls the row. A step's own label is what it does
  /// ("Ingest"), which says little on its own.
  shownName?: (row: R) => string;
};

export function rowActions<R extends ActionRow>(h: RowActionHost<R>) {
  const shown = h.shownName ?? ((r: R) => r.name.label);

  /// The steps a group row stands for.
  const stepsUnder = (group: ManageRow): R[] =>
    h.rows().filter((r) => r.kind === "step" && r.group === group.id);

  /// The row whose log answers for this one: a group's status child.
  const logRow = (row: R): R | undefined =>
    row.kind === "group"
      ? h.rows().find((r) => r.kind !== "group" && r.id === row.status_from)
      : row;

  async function act(what: () => Promise<void>) {
    h.busy.value = true;
    h.clear();
    try {
      await what();
    } catch (e) {
      h.say(false, (e as Error).message);
    } finally {
      h.busy.value = false;
    }
  }

  // ── One step's log. A red status says *that* a step failed; the next
  // question is always what it was doing.

  /// The run whose log answers "what was this step doing": the one in
  /// flight if the step is in it, else the one its record names, else —
  /// for a record from before runs had ids — the newest run the store
  /// says it took part in.
  async function runFor(row: R): Promise<{ runId: string; live: boolean } | null> {
    if (row.live_run_id) return { runId: row.live_run_id, live: true };
    if (row.last_run_id) return { runId: row.last_run_id, live: false };
    const [newest] = await h.api.fetchRuns({ step: row.id, limit: 1 });
    return newest ? { runId: newest.run_id, live: !newest.finished_at_utc } : null;
  }

  /// A step's log as a card beside this one; a group's is its status
  /// child's. With `runId`, that run's; without, the run in flight if the
  /// step is in it, else the one it last took part in.
  async function openStepLog(target: R, runId: string | null = null) {
    const row = logRow(target);
    if (!row) return;
    try {
      const current = h.run();
      const run = runId
        ? { runId, live: !!current?.live && current.run_id === runId }
        : await runFor(row);
      if (!run) {
        pushToast("This step has not taken part in any run the store remembers.");
        return;
      }
      // Open at the line that says how the step ended, for a row whose
      // status is the outcome of a run — the hover on Failed or Stopped
      // promises exactly that.
      const jumpToEnd = !run.live && !runId && ["failed", "stopped"].includes(row.status.key);
      h.host.openCards(logSource({ run: run.runId, step: row.id, jumpToEnd }));
    } catch (e) {
      pushToast((e as Error).message);
    }
  }

  /// The problems behind a row's count, as a grid over the index's
  /// `problems` table, its search bar holding the row's source. A step's
  /// problems are its group's — the render store is where a source's
  /// live — so a step row opens the same grid as its group. The index
  /// group shows every source's.
  function openProblems(row: R, problemsUrl: string) {
    const sourceId = row.kind === "group" ? row.id : (row.group ?? row.id);
    const q = sourceId === "unified_index" ? "" : `source_id:${sourceId}`;
    const source =
      row.kind === "group" ? row : h.rows().find((r) => r.kind === "group" && r.id === sourceId);
    const name =
      sourceId === "unified_index" ? "Problems" : `Problems: ${source?.name.label ?? sourceId}`;
    const opts = {
      url: problemsUrl,
      q,
      name,
      placeholder: "search problems…  (try: severity:error, -stage:fetch, after:2026-01-01)",
    };
    h.host.openCards(`gridView(${JSON.stringify(opts)})`);
  }

  /// Rows' commit history, as a card beside this one. On one source it is
  /// also where two of its versions are compared; `compare` opens it with
  /// the newest two set up.
  function openHistory(targets: R[], compare: boolean) {
    const [first] = targets;
    const source =
      targets.length === 1 && notComparableReason(menuTarget(first, null)) === null
        ? first.id
        : null;
    h.host.openCards(
      historySource({
        trees: targets.map((r) => r.id),
        title: targets.map((r) => r.name.label).join(", "),
        source,
        compare: compare && source !== null,
      }),
    );
  }

  /// One group's sync laid out as a dashboard, beside this card; from a
  /// step, its group's, scrolled to that step.
  function openDashboard(row: R) {
    const group = row.kind === "group" ? row.id : row.group;
    if (!group) return;
    h.host.openCards(
      syncDashboardSource({ group, step: row.kind === "step" ? row.id : undefined }),
    );
  }

  /// Leave for this row's data: one card, the grid, already filtered to
  /// the source and carrying its type's columns — or, for a download
  /// step in the app, its raw store in another program.
  function openBrowse(row: R) {
    if (row.rawStore) {
      const path = row.rawStore;
      void openRawStore(path).then((res) =>
        res.ok
          ? h.say(true, `Opened ${path} read-only in ${res.openedIn}.`)
          : h.say(false, `Could not open ${path}: ${res.reason}`),
      );
      return;
    }
    if (row.browseSource) h.host.openCards(row.browseSource);
  }

  /// Show a path where it lives, and say so when that fails rather than
  /// doing nothing.
  async function revealPath(path: string) {
    const ok = await revealInFileManager(path);
    if (!ok) h.say(false, `Could not open ${path} in the file manager.`);
  }

  /// Several rows at once, so their downstream steps run once. A step is
  /// its own seed; a group's seeds are its source steps. The server
  /// opens one request per source among them, each with its own Stop.
  async function runRows(targets: R[]) {
    const seeds = [...new Set(targets.flatMap((r) => r.seeds))];
    if (seeds.length === 0) return;
    const names = targets.map(shown).join(", ");
    await act(async () => {
      const requests = await h.api.openRequest(seeds);
      h.say(
        true,
        `Queued a sync for ${names}.`,
        requests.map((r) => r.id),
      );
      // The loop's record moving refetches too; this is for a page whose
      // stream is down.
      await h.reload();
    });
  }

  /// Sync everything the config declares: one request per source, in
  /// one run.
  async function runEverything() {
    await act(async () => {
      const requests = await h.api.openRequest([]);
      h.say(
        true,
        "Queued a sync of everything.",
        requests.map((r) => r.id),
      );
      await h.reload();
    });
  }

  /// Stop requests. Their steps checkpoint and exit; the rows say
  /// Stopping until they have.
  async function stopSyncs(requestIds: string[]) {
    if (requestIds.length === 0) return;
    await act(async () => {
      for (const id of requestIds) await h.api.stopRequest(id);
      h.say(
        true,
        `Stopping ${requestIds.length === 1 ? "the sync" : `${requestIds.length} syncs`}. ` +
          "Steps in flight checkpoint what they have and exit.",
        requestIds,
      );
      await h.reload();
    });
  }

  /// Turn off or on what these rows stand for: a step itself, a group
  /// every step under it.
  async function setTurnedOff(targets: R[], off: boolean) {
    const steps = targets.flatMap((t) => (t.kind === "group" ? stepsUnder(t) : [t]));
    await act(async () => {
      for (const s of steps) await (off ? h.api.turnOffStep(s.id) : h.api.turnOnStep(s.id));
      await h.reload();
    });
  }

  /// The steps a reset of these rows empties: a step is itself; a group
  /// is its download, what it renders following — or, for a comparison,
  /// which downloads nothing, its render. A download's blob store keeps
  /// its bytes (`docs/dev/step_protocol.md` § Reset).
  function resetTargets(targets: R[]): string[] {
    const steps = targets.flatMap((t) => {
      if (t.kind !== "group") return [t];
      const under = stepsUnder(t);
      const downloads = under.filter((r) => r.function === "ingest");
      return downloads.length ? downloads : under.filter((r) => r.function === "render_markdown");
    });
    const ids = steps
      .filter((r) => r.function === "ingest" || r.function === "render_markdown")
      .map((r) => r.id);
    return [...new Set(ids)];
  }

  /// Empty what these rows wrote, keeping the history. A render is
  /// rebuilt from what it reads at once; a download is not refilled, but
  /// what reads it catches up, so its documents leave the grid. The
  /// server runs it once no sync is running, and refuses it while one is.
  async function resetRows(targets: R[]) {
    const ids = resetTargets(targets);
    const names = targets.map((t) => t.name.label).join(", ");
    if (ids.length === 0) {
      h.say(false, `Nothing under ${names} keeps anything to reset.`);
      return;
    }
    const download = ids.some((id) => h.rows().find((r) => r.id === id)?.function === "ingest");
    const what =
      `Reset ${names}?\n\n` +
      `Every row goes, and the history keeps them. ` +
      (download
        ? `Its documents leave the grid, and the next Sync downloads it all again from nothing. ` +
          `Attachments already downloaded are kept.`
        : `Its documents are rendered again from what it has downloaded, now.`);
    if (!(await confirmAction(what))) return;
    await act(async () => {
      h.say(true, `Resetting ${names}…`);
      await h.api.resetSteps(ids);
      h.say(true, `Reset ${names}.`);
      await h.reload(true);
    });
  }

  /// A menu entry this module can carry out; false for one that needs
  /// the config (edit, rename, remove, compare), which the caller does.
  async function runMenuAction(action: MenuAction, targets: R[]): Promise<boolean> {
    const [first] = targets;
    switch (action) {
      case "browse":
        openBrowse(first);
        return true;
      case "sync":
        await runRows(targets);
        return true;
      case "stop":
        // One stop per request: several rows can be wanted by the same one.
        await stopSyncs([...new Set(targets.flatMap((t) => t.stop_request_ids))]);
        return true;
      case "turn_off":
      case "turn_on":
        await setTurnedOff(targets, action === "turn_off");
        return true;
      case "copy_id":
        await copyToClipboard(targets.map((t) => t.id).join("\n"));
        return true;
      case "copy_path":
        await copyToClipboard(
          targets
            .map((t) => t.reveal_path)
            .filter((p): p is string => !!p)
            .join("\n"),
        );
        return true;
      case "log":
        await openStepLog(first);
        return true;
      case "dashboard":
        openDashboard(first);
        return true;
      case "history":
        openHistory(targets, false);
        return true;
      case "reveal":
        for (const t of targets) if (t.reveal_path) await revealPath(t.reveal_path);
        return true;
      case "reset":
        await resetRows(targets);
        return true;
      case "edit":
      case "compare":
      case "rename":
      case "remove":
        return false;
    }
  }

  /// What each Actions-cell button does. The rows say which buttons a
  /// row carries and whether each is enabled; this is the code behind
  /// the id.
  const buttons: Record<string, (row: R) => void> = {
    browse: (row) => openBrowse(row),
    dashboard: (row) => openDashboard(row),
    sync: (row) => void runRows([row]),
    stop: (row) => {
      if (row.stop_request_ids.length > 0) void stopSyncs(row.stop_request_ids);
    },
    in_syncs: (row) => {
      const on = row.actions.find((a) => a.id === "in_syncs")?.on;
      void setTurnedOff([row], !!on);
    },
  };

  return {
    buttons,
    runMenuAction,
    runEverything,
    stopSyncs,
    openStepLog,
    openProblems,
    openHistory,
    openDashboard,
    revealPath,
  };
}
