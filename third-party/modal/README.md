# Modal's JavaScript SDK

npm's `modal` (Apache-2.0, <https://github.com/modal-labs/libmodal>),
which `datalib-step embed` drives to embed a source on the person's own
Modal account when its `embed_on_modal_from_n_chunks` says so
(`docs/dev/plans/embed_on_modal.md`). `runtime/` pins it with a lockfile
and Bazel builds the tree from that, as `third-party/latchkey/runtime/`
does for latchkey; `scripts/stage_runtime.sh` stages it into the runtime
as `modal/<version>/`.

The npm package ships no license file, so `LICENSE` here is the
repository's, verbatim, and the runtime carries it as
`modal/<version>/LICENSE`. The optional native addon `cbor-extract` is
left out of the lockfile (`overrides` in `runtime/package.json`): `cbor-x`
falls back to JavaScript without it, so the tree has no compiled code.
