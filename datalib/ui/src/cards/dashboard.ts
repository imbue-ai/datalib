// What the Dashboard's sections decide, as pure functions over the manage rows:
// which colour a status reads in, and what needs a person.
import type { ManageRow } from "@/api";

export type Tone = "ok" | "run" | "warn" | "error" | "muted";

/// A status key (manage/status.rs) as the colour it reads in.
export function statusTone(key: string): Tone {
  switch (key) {
    case "succeeded":
    case "skipped_up_to_date":
      return "ok";
    case "running":
    case "queued":
      return "run";
    case "failed":
    case "config_rejected":
      return "error";
    case "stopped":
    case "interrupted":
    case "blocked":
    case "config_blocked":
      return "warn";
    default:
      return "muted";
  }
}

/// A problems chip carries a bare count; this says what it counts.
function countOf(chip: ManageRow["problems"][number]): string {
  const noun = chip.kind === "error" ? "error" : "warning";
  return `${chip.text} ${noun}${chip.text === "1" ? "" : "s"}`;
}

export type Attention = {
  row: ManageRow;
  // What happened, after the row's name: "sync failed 2 hours ago".
  text: string;
  // Its last sync failed, so the log and a retry are what help.
  failed: boolean;
  // It holds errors or warnings, so the problems list is what helps.
  problems: boolean;
};

/// The groups a person should look at: one whose sync did not finish
/// well, or whose store holds errors or warnings. Failures first.
export function needsYou(groups: ManageRow[]): Attention[] {
  const out: Attention[] = [];
  for (const row of groups) {
    const tone = statusTone(row.status.key);
    const failed = tone === "error" || tone === "warn";
    const chips = row.problems.filter((c) => c.kind === "error" || c.kind === "warning");
    if (!failed && chips.length === 0) continue;
    const parts: string[] = [];
    if (failed) parts.push(`last sync: ${row.status.label.toLowerCase()}`);
    if (chips.length) parts.push(`has ${chips.map(countOf).join(" and ")}`);
    out.push({ row, text: parts.join("; "), failed, problems: chips.length > 0 });
  }
  return out.sort((a, b) => Number(b.failed) - Number(a.failed));
}
