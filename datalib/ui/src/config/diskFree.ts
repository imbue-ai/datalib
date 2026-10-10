// What the status bar says about free disk space: its hover, and when it
// raises or clears the toast that says syncs are paused. The loop holds
// every step from under the config's `[disk_space]` pause line until the
// resume line (`datalib/backend/dag/src/disk_space.rs`); this only
// reports it.

import type { DiskFree } from "@/api";
import { formatBytes } from "./bytes";

/// What changed since the last answer: the steps were held (or already
/// were when the page opened), were let go, or neither. `wasLow` is null
/// before the first answer.
export function diskCrossing(wasLow: boolean | null, disk: DiskFree): "low" | "cleared" | null {
  if (disk.low && wasLow !== true) return "low";
  if (!disk.low && wasLow === true) return "cleared";
  return null;
}

export function lowDiskMessage(disk: DiskFree): string {
  const free = formatBytes(disk.available_bytes ?? 0);
  return (
    `Only ${free} free on the data root's disk. Syncs are paused and running ` +
    `steps were stopped; they carry on once it has ${formatBytes(disk.resume_at_bytes)} ` +
    `free (the config's [disk_space]).`
  );
}

export const CLEARED_MESSAGE = "The data root's disk has room again; syncs carry on.";

export function diskTitle(disk: DiskFree, change: string): string {
  const free = formatBytes(disk.available_bytes ?? 0);
  const of = disk.total_bytes ? ` of ${formatBytes(disk.total_bytes)}` : "";
  const pause = formatBytes(disk.pause_below_bytes);
  const resume = formatBytes(disk.resume_at_bytes);
  const floor = disk.low
    ? `Syncs are paused until ${resume} is free.`
    : `Syncs pause under ${pause} free and carry on from ${resume}.`;
  return `${free} free${of} on the data root's disk.\n${floor}\n${change}`;
}
