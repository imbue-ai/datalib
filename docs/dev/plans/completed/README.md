# Completed plans

Plans that **landed**, kept as the record of what was decided and why.
They are not current reference: each one describes the design at the
moment it was built, and the tree has moved since. Read the banner
first, and check any claim against the code before repeating it.

Where a doc goes when it stops being a plan:

| | |
|---|---|
| still intended, not built | [`docs/dev/plans/`](../) |
| **landed, recently** | **here** |
| the best current explanation of how something works | `docs/dev/`, rewritten as reference |
| no longer worth keeping | deleted — git history has it |

The last row is the one worth pausing on. A completed plan that people
would genuinely read to *learn the system* belongs in `docs/dev/` as a
reference doc, not here — but that means rewriting it to describe what
is, rather than what was going to be. Filing it here instead is the
honest option when nobody is going to do that rewrite.

| Doc | What it was |
|-----|-------------|
| [`step_identity.md`](step_identity.md) | Making a step's `id` the path it writes, so `inputs` name step ids and the places that recovered an identity by splitting a string go away. Built 2026-08-31. Written as the design and kept as the explanation — where it says "proposal", read "what was done". |
| [`notion_redesign.md`](notion_redesign.md) | The case for rebuilding the Notion provider on the API Notion has now, and the live-workspace measurements behind it. Landed; the provider's current shape is in its [`DOWNLOAD.md`](../../../../datalib/backend/etl/providers/notion/DOWNLOAD.md). |
| [`provider_crate_split.md`](provider_crate_split.md) | Separating download from render so a render-schema change stops rebuilding every downloader. Built 2026-09-09: a provider is now two crates, and `datalib_schema` is unreachable from the download side (105 test targets downstream of it, now 79). Holds the measurements and the three things the proposal got wrong; the rules it left behind are in [`AGENTS.md`](../../../../AGENTS.md#download-and-render-are-separate-crates). |
