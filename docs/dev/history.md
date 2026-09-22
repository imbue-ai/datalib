# Repo history: what git cannot tell you

Facts about how this tree came to be that a reader cannot recover from
`git log` alone. Add a note here when you learn one; keep each to a
paragraph. Dated audits live in their own `audit_*.md` files.

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
