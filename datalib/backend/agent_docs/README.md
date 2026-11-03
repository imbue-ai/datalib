# The docs `datalib-step help` prints

An agent setting datalib up has a release and a shell, not the repo.
Every doc it is sent to is compiled into `datalib-step`, listed in
`src/lib.rs` (`DOCS`), and printed whole by `datalib-step help
<topic>`; `datalib-step help docs` lists them. A release writes each
beside the binaries, at its path in the repo
(`scripts/release/stage_tarball.sh`).

## Links

A doc is printed for a reader with no repo, so its links change:

- A link to another doc here prints as the command that prints it:
  ``[the runner's README](../../datalib/backend/dag/README.md)`` reads
  ``the runner's README (`datalib-step help runner`)``. An anchor names
  its heading, and an anchor into a section of
  `getting_your_data.md` that declares a source type prints as
  `datalib-step help <type>`, which is that section alone.
- A link to any other file in the repo prints as the raw file at the
  build's commit (`raw.githubusercontent.com`), never as a GitHub page,
  which is many times the file's size.
- A URL is left as written.

A **guide** (`Kind::Guide`, a doc written for someone using datalib)
links only to docs here, or to a contributor file named in
`GUIDE_LINKS_SERVED_RAW` in `src/tests.rs`. A doc an agent needs goes
in `DOCS` instead. A **reference** (written for contributors too) may
link anywhere. Every anchor into a doc here must name a heading.

## Examples are checked

- A ` ```toml ` block must load and plan as a config, the way `datalib-dag
  --check` would, with a stand-in step for each input it names and does
  not declare (`datalib_step`'s `agent_docs_test.rs`). Part of a config
  (one params table, say) is marked ` ```toml fragment `, and is only
  parsed as TOML.
- A command in a shell block (` ```sh `, ` ```bash `, or no language)
  that runs `datalib-dag`, `datalib-step`, `datalib-applet`,
  `datalib-migrate-config` or `datalib-http` is put through that
  program's own parser by its crate's tests, wherever on the line it
  starts. `datalib-doltlite` is not: its argv is sqlite3's. A
  `datalib-step help <topic>` a doc names must be a page.

## Adding a doc

Add a `Doc` to `DOCS`, the file to `compile_data` in `BUILD.bazel`, and
the file to its package's `exports_files`. Its topic must not be a
source type, a function or a command (a test says so).
