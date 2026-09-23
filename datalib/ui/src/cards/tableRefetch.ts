// Which `root` frames a `tableView({ url })` card refetches on. A table
// card that refetched on every frame fed itself: its request is a line
// in the server's log, and the log moving is a frame (`loop_guard.rs`
// on the server counts that, and warns).
import { changed, type RootEvent } from "@/live";

export function refetchesOn(url: string, e: RootEvent): boolean {
  const path = url.split("?")[0];
  // The unified-index applet serves from the index: the grid's rows,
  // the `problems` table.
  if (path.startsWith("/applet/unified_index/")) return e.kind === "index_changed";
  if (path === "/api/manage/rows") return changed(e, "manage.rows");
  // No frame names the rest (the remote-media tables); any frame is a
  // cue to look again, except the `log` one, which a request of our own
  // causes.
  return !changed(e, "log");
}
