* **Before open-sourcing: confirm the purged manual-e2e data is gone server-side.**
  The history rewrite itself is DONE. `configs/thad_tiny.yaml` and the 468
  `.snap` files under the old `backend/sync/tests/snapshots/` left the working
  tree in 26412853 (they live in the private `data_liberation_manual_e2e_test_data`
  dir) and were later expunged from history with `git filter-repo` — no reachable
  commit on `main` or `origin/main` contains either path. Two residual risks
  remain before the repo is made public: GitHub still holds the pre-rewrite
  blobs as unreachable objects, addressable by SHA until a support request
  GCs them; and any collaborator who never re-cloned still has them locally.
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
  (`DEFAULT_QMD_VERSION` in `unified_index/src/qmd/mod.rs`): the digest is
  theirs, changing it orphans every existing index, and the vendored tree
  under `third-party/qmd/` is reference-only. This is a note for whoever
  does the Rust re-implementation AGENTS.md §"Vendored upstream" already
  points at — not a reason to touch the interop code now, where SHA-256 is
  correct and load-bearing.
