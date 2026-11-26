// The sources `datalib-step --describe` offers, as the running app reads
// them from `GET /api/programs`: `builtin_sources.json` is that answer,
// written by the `:builtin_sources` genrule beside these tests. Every unit
// test starts with them in the catalog (`setupFiles` in vite.config.ts).

import type { ProgramDescription, ProgramsResponse } from "../src/api";
import { filterCatalog, type CatalogEntry } from "../src/config/catalog";
import * as byPath from "../src/config/programs";
import * as byAlias from "@/config/programs";

const describedPath = "./builtin_sources.json";
const described: ProgramDescription = (await import(/* @vite-ignore */ describedPath)).default;
const asServed: ProgramsResponse = {
  programs: [{ command: "datalib-step", description: described, error: null }],
};

// Under Bazel the `@` alias names `src/` by its real path in bazel-out and
// a relative import by its path in the runfiles tree, and vite loads a
// module once per path. A component reaches the catalog through the
// alias and a test through a relative path, so both copies are filled.
byPath.useDescriptions(asServed);
byAlias.useDescriptions(asServed);

/// Every built-in source, in the order `sources.toml` lists them: the
/// entries the catalog a test imports holds.
export const CATALOG: CatalogEntry[] = filterCatalog("");
