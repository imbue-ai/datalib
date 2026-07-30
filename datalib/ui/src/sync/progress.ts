// Parse the worker's task-board progress message. The DAG runner has
// no pipeline "stages" — a run is a set of tasks in todo / running /
// terminal states — so the worker publishes the whole board as JSON in
// `progress_msg` (`{"v":1,"tasks":[{"id","state","detail"?},…]}`). The
// UI renders it as one cell per task: green/red for done (by outcome),
// flashing yellow for running, blank for todo. Non-JSON messages
// (queue placeholders like "syncing …", legacy rows) yield null and
// the caller falls back to an indeterminate bar.

import type { SyncTask } from "@/api";

export function parseTasks(msg: string | null | undefined): SyncTask[] | null {
  if (!msg || !msg.startsWith("{")) return null;
  try {
    const v = JSON.parse(msg) as { v?: number; tasks?: unknown };
    if (v.v !== 1 || !Array.isArray(v.tasks)) return null;
    const tasks: SyncTask[] = [];
    for (const t of v.tasks) {
      const o = t as { id?: unknown; state?: unknown; detail?: unknown };
      if (typeof o.id !== "string" || typeof o.state !== "string") return null;
      tasks.push({
        id: o.id,
        state: o.state,
        detail: typeof o.detail === "string" ? o.detail : undefined,
      });
    }
    return tasks.length > 0 ? tasks : null;
  } catch {
    return null;
  }
}
