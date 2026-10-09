// What a picker says about its list — while it loads, from how far it
// has got and how long it has taken, and once loaded, how many came.
import type { ProbeProgress } from "@/api";

/// "Loading channels… 400 so far · 12s", "… 120 of 480 · 3s", or with
/// nothing counted yet just the elapsed time. The seconds appear from
/// the first whole one, so a fast load never shows a clock.
export function loadingText(noun: string, progress: ProbeProgress | null, seconds: number): string {
  let text = `Loading ${noun}…`;
  if (progress && progress.done > 0) {
    text +=
      progress.total !== null
        ? ` ${progress.done.toLocaleString()} of ${progress.total.toLocaleString()}`
        : ` ${progress.done.toLocaleString()} so far`;
  }
  const whole = Math.floor(seconds);
  return whole >= 1 ? `${text} · ${whole}s` : text;
}

/// "1 channel", "3 channels". Every list noun the wizard uses is a
/// regular plural — channels, conversations, labels, folders,
/// calendars, address books.
export function countOf(n: number, plural: string): string {
  return `${n.toLocaleString()} ${n === 1 ? plural.replace(/s$/, "") : plural}`;
}
