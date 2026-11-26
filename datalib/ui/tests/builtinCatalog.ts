// The sources `datalib-step --describe` offers, as the running app reads
// them from `GET /api/programs`: `builtin_sources.json` is that answer,
// written by the `:builtin_sources` genrule beside these tests. Every unit
// test starts with them in the catalog (`setupFiles` in vite.config.ts).

import type { ProgramDescription } from "../src/api";
import * as byPath from "../src/config/catalog";
import * as byAlias from "@/config/catalog";
import { entriesOfDescription } from "../src/config/programs";

const describedPath = "./builtin_sources.json";
const described: ProgramDescription = (await import(/* @vite-ignore */ describedPath)).default;

/// Every built-in source, in the order `sources.toml` lists them.
export const CATALOG: byPath.CatalogEntry[] = entriesOfDescription("datalib-step", described);

// Under Bazel the `@` alias names `src/` by its real path in bazel-out and
// a relative import by its path in the runfiles tree, and vite loads a
// module once per path. A component reaches the catalog through the
// alias and a test through a relative path, so both copies are filled.
byPath.setProgramEntries(CATALOG);
byAlias.setProgramEntries(CATALOG);
