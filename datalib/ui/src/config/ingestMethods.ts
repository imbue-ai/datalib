// How an ingest step brings data in — from a live service ("origin") or
// from files already on this machine ("local") — read off the step's
// written params against the methods its provider declares. The
// declarations live in the provider config crates (`IngestMethods` in
// datalib_source_common); `ingestMethods.json` beside this file is
// generated from them by
// `bazel run //datalib/backend/datalib_step:ingest_methods.update`, and
// a test there fails when it drifts, so nothing here is a hand-kept copy.
//
// The rule is `datalib-step`'s (`methods.rs`): a method is held when its
// path is written and its value is neither null nor false — a table
// counts by presence (`sync = {}` is a complete selection), a flag such
// as linkedin's `fetch_photos` only when on.
import DECLARED from "./ingestMethods.json";

export type Reach = "origin" | "local";

export type IngestMethod = {
  /// Dotted path into the step's params: `sync`, `gmail_api`, `common.input_path`.
  path: string;
  reach: Reach;
};

export const INGEST_METHODS: Record<string, IngestMethod[]> = DECLARED as Record<
  string,
  IngestMethod[]
>;

/// The methods a group `type` accepts; empty for a type this build has
/// no provider for.
export function methodsOf(type: string | null | undefined): IngestMethod[] {
  return (type && INGEST_METHODS[type]) || [];
}

function valueAt(params: Record<string, unknown>, path: string): unknown {
  let cur: unknown = params;
  for (const seg of path.split(".")) {
    if (cur === null || typeof cur !== "object" || Array.isArray(cur)) return undefined;
    cur = (cur as Record<string, unknown>)[seg];
  }
  return cur;
}

export function methodsHeld(
  type: string | null | undefined,
  params: Record<string, unknown>,
): IngestMethod[] {
  return methodsOf(type).filter((m) => {
    const v = valueAt(params, m.path);
    return v !== undefined && v !== null && v !== false;
  });
}

/// "origin" if anything held reaches one, else "local", else null — the
/// step names no method, or its type is unknown here.
export function ingestReach(
  type: string | null | undefined,
  params: Record<string, unknown>,
): Reach | null {
  const held = methodsHeld(type, params);
  if (held.some((m) => m.reach === "origin")) return "origin";
  return held.length ? "local" : null;
}

/// The word the Manage row uses for an ingest step: "Download" for a
/// method that reaches an origin, "Import" for one that reads files on
/// disk. Null when the params name no method the provider declares.
export function ingestLabel(
  type: string | null | undefined,
  params: Record<string, unknown>,
): string | null {
  switch (ingestReach(type, params)) {
    case "origin":
      return "Download";
    case "local":
      return "Import";
    default:
      return null;
  }
}
