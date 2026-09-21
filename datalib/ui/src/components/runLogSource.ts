// Where a log line's `filename` and `line_number` point: the link the
// run-log panel builds, at render time, from what the store keeps (the
// path rustc saw and the commit the process came from).

export type Source = { file: string; line: number | null };

/// Where a line's file and line number point.
export const SOURCE_REPO = "https://github.com/imbue-ai/datalib";

/// The `filename` and `line_number` a tracing line carries, out of its
/// `fields` JSON; nothing for a plain line. The path is the one rustc
/// saw: repo-relative for our crates, except that a crate bazel compiles
/// from a copy (one with generated inputs) reports the copy's
/// `bazel-out/<config>/bin/` prefix, dropped here so the file is the
/// same file either way.
export function sourceOf(fields: string | null | undefined): Source | null {
  if (!fields) return null;
  let parsed: Record<string, unknown>;
  try {
    parsed = JSON.parse(fields) as Record<string, unknown>;
  } catch {
    return null;
  }
  const raw = parsed.filename;
  if (typeof raw !== "string" || !raw) return null;
  const file = raw.replace(/^bazel-out\/[^/]+\/bin\//, "");
  const n = parsed.line_number;
  return { file, line: typeof n === "number" && Number.isFinite(n) ? n : null };
}

/// Whether a file is in this repo, so a link to it can mean anything: a
/// third-party crate's line names a path under `external/` or an
/// absolute one in a cargo registry.
export function inRepo(file: string): boolean {
  return !file.startsWith("/") && !file.startsWith("external/") && !file.startsWith("..");
}

export function sourceLabel(src: Source): string {
  return src.line == null ? src.file : `${src.file}:${src.line}`;
}

/// The line on GitHub at `commit`, or null for a file not in the repo.
export function sourceUrl(commit: string, src: Source): string | null {
  if (!inRepo(src.file)) return null;
  const path = src.file.split("/").map(encodeURIComponent).join("/");
  return `${SOURCE_REPO}/blob/${commit}/${path}${src.line == null ? "" : `#L${src.line}`}`;
}
