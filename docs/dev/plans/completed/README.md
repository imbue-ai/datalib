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

There is no index of them here, on purpose: a list every landing PR
appends to is a merge conflict waiting to happen. Each plan opens
with a banner saying what landed and where its reference now lives;
`head -n 8 docs/dev/plans/completed/*.md` reads them all.
