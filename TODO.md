* **Before open-sourcing: confirm the purged manual-e2e data is gone server-side.**
  The history rewrite itself is DONE. `configs/thad_tiny.yaml` and the 468
  `.snap` files under the old `backend/sync/tests/snapshots/` left the working
  tree in 26412853 (they live in the private `data_liberation_manual_e2e_test_data`
  dir) and were later expunged from history with `git filter-repo` — no reachable
  commit on `main` or `origin/main` contains either path. Two residual risks
  remain before the repo is made public: GitHub still holds the pre-rewrite
  blobs as unreachable objects, addressable by SHA until a support request
  GCs them; and any collaborator who never re-cloned still has them locally.
* **Surface upstream data loss in the UI.** The versioned raw store can
  answer "what did your provider quietly change or delete since last
  sync?" — a plain mirror can't, and it is close to the point of the
  project. Nothing surfaces it: grepping `datalib/backend/http` and
  `datalib/ui` for `deltas` returns zero hits (checked 2026-09-05), and
  the only human-visible form is `fsindex`'s standalone CLI printing
  `vs last scan: N added, M modified, K removed`. Wanted: a per-run
  "12 conversations vanished from claude.ai" line, and a way to click
  through to what they were.
  Three things stand in the way:
  1. **Coverage is 8 of 20.** Only beeper, chatgpt, claude, email,
     github, gitlab, notion and slack use `DownloadRun`, so only they
     write `sync_runs.summary.deltas`. The other twelve write no
     `sync_runs` row at all. Their history is still in the commits —
     `dolt_diff` between refs always works — but a UI would have to
     compute it rather than read it.
  2. **The providers that detect deletions and the providers that
     record deltas are nearly disjoint.** `media` and `fsindex` catch
     deletions structurally (truncate-and-refill) and record nothing;
     most of the eight that record deltas are cursor-driven walkers that
     never revisit old rows, so nothing lands in their diff no matter
     what happened upstream. Only `email` (tombstones) and
     `claude_export` (`prune_to`) sit in both today. The rest is an
     unwritten per-provider audit, and it gates the feature rather than
     merely informing it: reporting "0 deleted" for a provider that
     cannot detect deletions is worse than reporting nothing, so the UI
     has to distinguish "nothing was deleted" from "we would not know."
  3. **`sync_runs` records no commit hashes** (`run_id`, `started_at`,
     `finished_at`, `config`, `status`, `summary`). Stamping HEAD at
     start and finish makes a past run's exact diff a two-value lookup
     instead of an archaeology exercise, and is what (1) needs to
     backfill the twelve.
  Background and caveats:
  [`data_architecture_ingestion.md` §"Noticing when the *upstream* loses
  data"](docs/dev/data_architecture_ingestion.md#noticing-when-the-upstream-loses-data).
  Related: `deleted_upstream_at`, which that doc specifies and which
  exists nowhere in the tree.
* Notion: The order of the blocks in this markdown looks wrong: /Users/thad/datalib.thad_tiny_1/rendered_md/notion/pages/364a550f-af95-80de-829f-c5fccb3021fd/index.md
* Make sure that markdown for Notion and Slack has relative links for other documents and media.
* **If we ever fork qmd (or re-implement it), switch its content hash to blake3.**
  qmd content-addresses with SHA-256 — `hashContent` is
  `createHash("sha256")` (`third-party/qmd/src/store.ts:2365`), and the
  digest is the key on `documents`, `content`, and `content_vectors`, plus
  the re-index decision (`existing.hash === hash`, store.ts:1332).
  Everything datalib hashes for itself uses blake3 instead
  (`blob_cas::blake3_hex`, `fswalk::hash_file`, the pdf provider's
  `blake3`), so today the qmd boundary is the one place we compute a
  second digest over bytes we have already hashed:
  `unified_index::qmd::index_state::file_sha256_hex` re-reads and
  re-hashes every rendered file the grid asks about, purely to match
  qmd's key. Unifying on blake3 collapses that — and the proposed
  `markdowns.md_sha256` column in `docs/dev/qmd_index_ui.md` becomes a
  blake3 column we may already be able to derive.
  **Not actionable while we consume `@tobilu/qmd` from the registry**
  (`DEFAULT_QMD_VERSION` in `runtime/src/qmd.rs`): the digest is
  theirs, changing it orphans every existing index, and the vendored tree
  under `third-party/qmd/` is reference-only. This is a note for whoever
  does the Rust re-implementation AGENTS.md §"Vendored upstream" already
  points at — not a reason to touch the interop code now, where SHA-256 is
  correct and load-bearing.
