# `third-party/qmd` is a reference snapshot, not what we run

`third-party/qmd/` is a checked-in snapshot of
[`github.com/tobi/qmd`](https://github.com/tobi/qmd) at the version in
`third-party/qmd/package.json`. It exists as a **reference for the qmd
format** — read-only documentation in code form. What we run is the
registry package at the pin in `datalib/backend/runtime/src/qmd.rs`
(`DEFAULT_QMD_VERSION`): the desktop app, the release tarball and the
dev launchers all carry a Node runtime plus the `qmd` and `latchkey`
package trees, all three produced by Bazel from lockfiles
(`//datalib/tauri:bundled_node`, `//third-party/qmd/runtime:qmd_tree`,
`//third-party/latchkey/runtime:latchkey_tree`, staged by
`scripts/stage_runtime.sh`). `npx -y @tobilu/qmd@<version>` survives
only as an opt-in dev fallback (`DATALIB_ALLOW_NPX=1`).

## Why we don't run from the vendored tree

Pointing the indexer at `third-party/qmd/bin/qmd` looks hermetic and is
not: the tree is source-only, so running it needs `pnpm install` (which
compiles `better-sqlite3`, `node-llama-cpp`, `sqlite-vec` and several
`tree-sitter-*` natives) and `pnpm run build`, plus node ≥22 and a C
toolchain on the host. That is "npx-free", not hermetic, and npx's cache
already makes repeat runs cheap. If we want better isolation later the
likelier direction is to re-implement the bits of qmd we use (indexing
and retrieval over our markdown tree) in Rust, using this tree as the
format reference.

## Bumping the snapshot

It was pulled in with `git subtree add --squash`, so upstream is one
squashed commit plus a merge in our history. To bump:

```sh
git subtree pull --prefix=third-party/qmd \
  https://github.com/tobi/qmd.git <new-tag> --squash
```

Do **not** edit files under `third-party/qmd/`; the next pull overwrites
them.
