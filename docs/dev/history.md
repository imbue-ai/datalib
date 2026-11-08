# Repo history: what git cannot tell you

Facts about how this tree came to be that a reader cannot recover from
`git log` alone. Add a note here when you learn one; keep each to a
paragraph. Dated audits live in [`audits/`](audits/).

## Two placeholder git identities

`git shortlog` without `.mailmap` shows two authors who are not people:

- **`Test <test@example.com>`, 123 commits, 2026-05-13 → 2026-05-28.**
  Thad, working in a Sculptor sandbox whose `user.name`/`user.email`
  were never set. Every one of them is on `main`, in Thad's timezone,
  interleaved with his own commits on the same work (the M1 Slack port,
  the ETL crate split, the chatgpt/anthropic port, JSONB payloads), and
  118 carry a `Co-authored-by: Sculptor` trailer. Qi's first commit is a
  week after the last of these.
- **`x <x@y.z>`, one commit, `37dfab80`, 2026-07-31.** Qi, from an `mngr`
  agent sandbox: a CI probe on the `mngr/musl-bridge` branch, between two
  of Qi's commits and merged by Qi as PR #105.

The root `.mailmap` maps both to their owners so `shortlog` and `blame`
read right. History was left as it is on purpose — rewriting `main` is
off the table (see "Git: prefer merges over rebases" in `AGENTS.md`).

## blake3 is built with its SIMD code, so never with `pure`

blake3's `pure` feature leaves out its C and assembly kernels: NEON on
arm64, AVX-512 on x86-64 (SSE and AVX2 survive there as Rust). On an
Apple M4 that halves hashing speed, and blake3 hashes every file the
fingerprint cache vouches for and every blob in the store. `pure` was
dropped on purpose in `175b51b3e` (Sep 30, 2026) and came back by
accident in `7966e3a6d` (Oct 9), when the per-crate `Cargo.toml` files
were folded into one; a profile of an ingest found it a month later.
The hashes are the same either way, so no stored hash moved.
`//datalib/backend/etl/files:blake3_simd_test` now fails to link
without the kernel. Turning NEON on surfaced a clash CI does not build:
doltlite embeds blake3's portable C, and on aarch64 the blake3 crate
exports `blake3_compress_in_place_portable` too, so the static musl
link saw it twice. `third-party/doltlite/BUILD.bazel` renames
doltlite's copy.
