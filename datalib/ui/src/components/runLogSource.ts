// Where a log line's `filename` and `line_number` point: the link the
// run-log panel builds, at render time, from what the store keeps (the
// path rustc saw and the commit the process came from).

export type Source = { file: string; line: number | null };

/// Where a line's file and line number point.
export const SOURCE_REPO = "https://github.com/imbue-ai/datalib";

/// The ref a link falls back to when the process that wrote the line
/// could not say which commit it was built from.
export const SOURCE_DEFAULT_REF = "main";

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

/// The fields JSON with `filename` and `line_number` taken out, for the
/// Fields column beside a Source column that shows them; empty when
/// nothing else was there.
export function fieldsWithoutSource(fields: string | null): string {
  if (!fields) return "";
  let parsed: unknown;
  try {
    parsed = JSON.parse(fields);
  } catch {
    return fields;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return fields;
  const rest = { ...(parsed as Record<string, unknown>) };
  delete rest.filename;
  delete rest.line_number;
  return Object.keys(rest).length === 0 ? "" : JSON.stringify(rest);
}

export function sourceLabel(src: Source): string {
  return src.line == null ? src.file : `${src.file}:${src.line}`;
}

/// The line on GitHub at `commit`, or at `main` when the commit is not
/// known; null for a file not in the repo, where no link means anything.
export function sourceUrl(commit: string | null | undefined, src: Source): string | null {
  if (!inRepo(src.file)) return null;
  const path = src.file.split("/").map(encodeURIComponent).join("/");
  const ref = commit || SOURCE_DEFAULT_REF;
  return `${SOURCE_REPO}/blob/${ref}/${path}${src.line == null ? "" : `#L${src.line}`}`;
}
