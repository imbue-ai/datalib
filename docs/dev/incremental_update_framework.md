# One incremental-update framework for every render

> **Proposal — nothing here is built.** It describes one shape for
> work that twelve providers currently do twelve ways, and names the
> four questions that shape has to answer. The audit it grew out of is
> in §2; that part is measurement and can be checked against the tree.
> Read [`provider_migration_dolt_diff_and_cas_edge.md`](provider_migration_dolt_diff_and_cas_edge.md)
> first — this replaces the render half of that recipe, and its
> per-provider notes stay accurate.

## 1. The shape

A render step consumes an upstream store by asking it what changed
since the commit that render last finished against. From the answer it
derives two lists, and processes them in this order:

1. **Documents to delete** — their upstream entity is gone.
2. **Documents to write** — added or changed. One UPSERT path; we never
   need to tell "new" from "modified" apart.

Deletes go first so that a run interrupted between the two halves has
already let go of what upstream no longer has, rather than leaving a
document pointing at nothing.

Only when everything above has landed does the cursor advance, and it
advances to **the commit the scan named**, not to the store's current
HEAD — those differ whenever somebody committed while we worked. An
interrupted run leaves the old cursor, so the next run re-processes
part of the same range. Every step must therefore be idempotent, which
it is: deleting a document that is already gone is a no-op, and writing
one that is already correct rewrites identical bytes.

That is the whole contract. The rest of this document is the four
places where it is harder than it looks.

## 2. Where we actually are

`build_grid_index` ([`grid_index.rs:590`](/datalib/backend/etl/src/grid_index.rs))
already implements the shape exactly, and is the reference: it asks
each render store `changed_since(cursor)`, treats "an id the diff named
that the store no longer has" as a deletion, deletes first, upserts
second, and advances `source_cursors` **inside the same write
transaction** — so the cursor can never claim more than the index
holds.

The twelve provider renderers do the same thing by hand, on top of
three shared primitives ([`render_cursor`](/datalib/backend/etl/src/render_cursor.rs),
`doltlite_raw::scan_buckets`, `doltlite_raw::buckets_without_rows`) and
two `RunCtx` sinks (`remove_conversation`, `retain_documents`). Checked
against `main` at `2ee0e2da` on 2026-09-08. The pin work (#314 through
#336) has closed §3.5 and all of §3.2's *bail* hazard; it left this
table exactly where it was:

| provider | cursor | how deletions are found | order | load narrowed |
|---|---|---|---|---|
| slack, signal | yes | `buckets_without_rows` | delete → write | yes, in SQL |
| email | yes | `buckets_without_rows` | delete → write | yes, in SQL |
| chatgpt, claude, notion, github, gitlab | yes | `buckets_without_rows` | delete → write | no — loads all rows, filters in memory |
| whatsapp | yes | `buckets_without_rows` | delete → write | no; scan and delete live inside `render_all`, and it opens a second reader pool |
| pdf | yes | ad hoc (membership in the target list, not a store query) | delete → write | no |
| sms_backup_restore | yes | its own `outcome.vanished` | **write → delete** | n/a |
| yolink | yes | none — one document per store, HEAD-gated | n/a | n/a |
| beeper | **no** | **none at all** — fingerprint skip only | — | no |
| contacts, perseus, google_takeout, linkedin | no | `retain_documents` whole-store sweep | **write → delete** | full re-derive |

Five providers (github, gitlab, pdf, sms_backup_restore, beeper) still
thread `ctx.prior_fingerprints` into render as a second, redundant skip
underneath the diff.

The deletion half has a test in exactly two places:
`github/tests/incremental_render.rs` and
`notion/tests/incremental_render.rs`.

## 3. The four hard parts

### 3.1 Which documents does a changed row touch?

The diff's unit is a row. Our unit is a document. Nothing today records
the relation between them, so every provider re-derives it in SQL, and
the derivation has three separate failure modes:

- **One row → one document.** Fine. A `bucket_query` projects the
  bucket key and we are done.
- **One row → many documents.** A `users` row reaches every document
  that names that person, and the store cannot say which. Handled today
  by `DiffScanSpec::global_fanout_tables`: any change to a listed table
  means "re-render everything." Slack lists `workspaces, users,
  channels`; claude lists `users, orgs, projects`; signal lists
  `recipients`; notion and chatgpt list one each. So in a Slack
  workspace where people join, **every run re-renders the whole
  workspace**.

  This has now been measured rather than predicted. #335 caught a live
  bake failing its stability check because one person's Slack status
  flipped to "In a meeting" mid-run — four fields, one user — and the
  fix moved `profile.status_*` and `profile.huddle_*` to the volatile
  sidecar so they stop counting as content. That removes one *source*
  of churn and not the amplification: `users` is still a global fanout,
  so any real edit (a display-name change, a new member) still re-renders
  every document in the workspace. The commit's own summary — "the render
  re-runs, and the grid churns" — understates it for exactly that
  reason.
- **Many rows → one document.** A page's comments, a PR's reviews. Each
  provider joins these back to the owning entity by hand, and a
  provider that cannot (contacts — one row holds several vCards, so a
  diff row does not name a document) is simply unportable. The
  migration doc calls this "the porting precondition, learned the hard
  way."

**Do we have the bookkeeping? Almost, and not the useful half.**
`grid_rows` carries `upstream_id` / `upstream_entity_kind` /
`upstream_scope` — a real backpointer from a row to the upstream thing
it came from — and `markdown_uuid` beside it. But only `pdf` populates
those columns today, and even fully populated they name the row a
document *is*, not the rows a document *read*. The fan-in — the case
that costs us — is unrecorded. `edges` does not help: it is
document→document.

**The proposal: record what each document was rendered from.** A fourth
table in the render store, written the same way `rows`, `edges` and
`problems` already are — as a field on `RenderedMarkdown`, so it
commits with the document:

```
render_deps(markdown_uuid, dep_table, dep_id, is_owner)
```

One row per upstream row the renderer actually read. The renderer
already resolved every one of them, so this is free to produce.

What it buys:

- **Fan-out becomes exact.** A changed `users` row expands to
  `SELECT DISTINCT markdown_uuid FROM render_deps WHERE dep_table='users'
  AND dep_id=?`. `global_fanout_tables` goes away, and with it the
  re-render-everything behaviour.
- **Fan-in stops needing a join.** A changed `comments` row names its
  document directly. Each provider's `bucket_query` shrinks to the
  tables that can *create* a document — notion's five-way union becomes
  two, and the child-table joins disappear.
- **Deletion stops needing a bucket-key → conversation_uuid recipe.** A
  document whose `is_owner` dep row is gone is a document to delete.
  That is `buckets_without_rows` generalized, and it dissolves the
  precondition that blocks contacts. It also removes claude's guess —
  today claude calls `remove_conversation` twice per vanished bucket,
  once as a conversation and once as a project, because the bucket id
  no longer says which it was.

What still needs the provider: a row in a table nothing depends on yet
is either a brand-new document or noise, and only the provider knows
which. So `bucket_query` survives, narrowed to the creating tables.

**The risk is the shape AGENTS.md warns about**: a dep we forget to
record does not fail, it silently stops re-rendering. Two guards — the
framework refuses a document with no `is_owner` dep, and a full pass
rebuilds every dep row from scratch (§3.2), so a bad deps table is at
worst one cold start from correct.

**On deferring names to UI time** (the other option): worth doing, and
orthogonal. It works for the structured columns — `grid_rows.author`
could hold the user id and the applet could join a names table, so a
rename shows up with no re-render at all. It does not work for the
markdown body, where the name is inline prose, and the `.md` is what
qmd indexed and what `/applet/unified_index/chat/{uuid}` serves. So it
narrows the blast radius rather than removing it, and it needs
`render_deps` anyway for the part it cannot cover.

### 3.2 "I could not diff" is "diff from empty"

Right, and this is the hole in what we have. Today the two states are
handled by different code: a diff-narrowed run *names* deletions, and a
cold-start run **deletes nothing at all** — every provider does
`gone = Vec::new()` when `changed_buckets` is `None`. Then it writes
the cursor anyway. So a deletion that happened inside a window we
could not diff is never revisited.

That window is not rare. It opens on: no cursor, render params changed
(`read_for_params` invalidates), `dolt_log()` unavailable, the diff
query erroring — **and on every global-fanout short-circuit**, which
for slack means every time somebody joins the workspace.

Under the proposal there is one code path. A pass that could not diff
declares itself a **full pass**, and a full pass is exactly "delete
everything, then upsert everything":

- delete every document in the store,
- render the whole upstream, upserting each document,
- rebuild `render_deps` from scratch as it goes,
- commit once, advance the cursor.

Cost is the same as today's cold start — the rendering was already
happening; only the deletes are new. And it is *safe* rather than
merely cheap, for two reasons worth stating out loud:

- **The store is transactional and the reader pins.** Everything above
  happens in one doltlite commit, and `grid_index` reads through
  `pin::Pin` at the last commit. A run that dies halfway is invisible
  downstream, and the next run repeats it because the cursor never
  moved.
- **Delete-then-reinsert of an unchanged row is not a diff.** The
  prolly-tree diff compares end states, so a full pass over an
  unchanged source produces an empty `dolt_diff` and the index does
  nothing. Worth an explicit test, because this is the property the
  whole idea rests on.

**The *bail* half of this is now closed** (#316, #322), and by the route
the framework wants. #316 gave the sweep a `RenderPass::{Walked, Skipped}`
it must be handed — a return value, not a flag, because the caller is not
the one who knows. #322 then fixed what the guard was still being lied
to about: five render paths turned "cannot read this store" into an empty
result set from an inner block and fell through reporting `Walked`, so
the value leaked past a guard that was testing the right thing. They
carry `Option` all the way out now, and contacts' `parse` returns
`Option<ParsedContacts>` rather than an empty one. The sentence #322
wrote down is the whole rule and is better than the way §3.2 states it
here: **a sink that cannot answer must say *that*, not hand back an
empty result and let the consumer draw the conclusion.**

**And the same class keeps producing new instances**, which is the
argument for making it structural rather than fixing it once per site.
Since that rule was written down, three more have surfaced:

- **pdf's `load_targets`** (#333). It doubles as the membership test
  behind `remove_conversation`, so an empty list from a store we could
  not read deletes everything the diff named. Worth noting how the fix
  had to be ordered: unpinned it read the working set and so saw *more*
  than committed, which under-deleted; pinning it *without* also
  returning `Option` would have inverted that into over-deletion.
- **A store with tables but no committed schema** (#334). A doltlite
  file is born with an "Initialize data repository" commit, so
  `dolt_hashof('HEAD')` resolved, `head` handed back a pin,
  `install_views` gave every table the empty `WHERE 0` view, the
  consumer read zero rows and reported a completed walk, and the sweep
  deleted the source. Nothing in that chain looks like an error, which
  is why it survived two rounds of work looking straight at it. Reachable
  by a download that created its tables and died before its first commit.
- **A writer that never sealed** (#336) — and this one is the limit of
  the rule as stated. Pinned at the schema commit, "empty source" and
  "writer wrote and never committed" produce *identical* reads. The sink
  genuinely cannot tell, so it cannot say. The only difference is a dirty
  `dolt_status`, which `install_views` now warns on — and that signal
  stops meaning anything under streaming, where a reader alongside a
  live writer sees a dirty store legitimately. Whatever replaces it has
  to come from the producer, not from the reader's inspection of the
  file.

What is left is the *cold-start* half, untouched: every diff-driven
provider still does `gone = Vec::new()` when `changed_buckets` is `None`
— see `github/src/render/parse.rs:255` and the same shape at
`notion/src/download/db.rs:736` — and then advances the cursor anyway.
A deletion inside a window we could not diff is still never revisited.

**And checkpointing has just made the "partial store" case real.** #329
lets a download seal mid-fetch, so render can now legitimately read a
store its producer is still writing. Two things follow for this section:

- The reset carve-out is right and narrower than it looks.
  `RunCtx::checkpoint_policy` returns `Policy::Never` under
  `reset_and_redownload`, because a store mid-truncate-and-refill is
  indistinguishable from a source that lost most of its data. That is
  exactly the reasoning above.
- **But the flag is not the condition.** A checkpoint is only safe where
  a run's writes are monotone, and three providers truncate on *every*
  run regardless of the flag: whatsapp (every backup is a full
  snapshot), pdf ("the truncate is what makes deletions fall out") and
  fsindex. Their consistent point is "after the refill completes", not
  "after a write burst goes quiet". #333 wrote this down beside
  `Policy::Never` in `checkpointer.rs`, which is where whoever ports the
  next provider will be looking.

**One caveat, and it is a real one.** The `.md` files are not in the
transaction. `IndexedMarkdownStore::remove_document` unlinks the file
immediately ([`indexed_markdown.rs:179`](/datalib/backend/etl/src/indexed_markdown.rs)),
so a naive "delete everything first" would unlink the whole tree and a
crash mid-pass would leave committed rows pointing at files that are
gone — the applet 404s until the next successful render. Today's
retain-sweep-at-the-end avoids this by accident. So on the full-pass
path the deletion must be **rows only**, with an unlink sweep after the
commit, dropping the files no surviving `markdowns.md_path` claims.
That sweep is the one piece of this that must be written from nothing.

### 3.3 A shrunk bucket is a re-rendered bucket

Yes — and the primitive we are missing is a *scoped* retain, not a
different kind of delete.

`remove_conversation` is all-or-nothing: it drops every document of a
conversation. There is nothing that says "these are all the documents
for conversation X; drop anything else you hold under it." So when a
conversation survives but produces fewer documents than last time, the
extras are orphaned. Concretely: delete January's messages from a live
chat and `.../2026-01/all.md` stays in the grid forever.

Both halves already exist —
`documents_for_conversation(uuid)` (which joins on
`grid_rows.conversation_uuid`) and the emitted set the renderer has in
hand. The framework should close the loop itself: after finishing a
bucket, delete `documents_for_conversation(scope) − emitted`.

Scope: this only bites providers that shard one conversation across
several documents — **signal, whatsapp, beeper, sms_backup_restore**
(and google_takeout). Slack renders one document per thread
(`period_key: "all"`), and **email is one document per thread, as it
should be** — `email/src/render/render.rs:441` sets `period_key: "all"`.
Nothing to fix there.

The provider supplies one function: bucket key → the conversation
uuid(s) that bucket owns. Every processor already computes this inline
before calling `remove_conversation`; claude and notion return two
(conversation + project; page + its discussions).

### 3.4 The cursor belongs in the store

Agreed, and the reason is sharper than tidiness: there are currently
two finish lines and they fire in the wrong order.
`render_cursor::write` lands a JSON file at the end of the provider's
`render_all`; the render store's `dolt_commit` happens afterwards, back
in the step driver ([`datalib_step/src/render.rs`](/datalib/backend/datalib_step/src/render.rs)).
Crash between them and the cursor claims a commit the store never
recorded. It self-heals — doltlite's working set persists and the next
run's `-Am` sweeps it up — but by accident, not by design.

`source_cursors` on the index side is the shape to copy: a row in the
store being written, updated in the same transaction as the deletes and
the upserts, so it cannot get ahead of them. Add the equivalent table
to `indexed_markdown.doltlite_db`, holding the raw store's commit hash
and the render params hash that `read_for_params` compares.

Two things fall out of the move. The step driver's
`rendered_tree_version` currently derives the DAG output version by
reading that JSON file, and would read the store instead — an
improvement, since the store's own commit hash is the more honest
answer. And `--reset-and-redownload` gets the cursor wipe the migration
doc recommended and nobody implemented: with the cursor in the store,
"reset" is just a full pass.

### 3.5 Readers always pin

Substantially done across #314, #316, #319, #322, #326 and #328, and the
shape it settled into is the one to copy elsewhere.

**The pin lives on the handle.** `RawDb::open_reader` samples HEAD,
installs the `pinned_<table>` views and keeps the pin; every loader reads
through `self.reads()`. That is where the guarantee actually holds — the
views are per-connection, so a pin was never a per-call property, and the
earlier `Reads::At(pin)` signature promised one it could not provide.
There is now no constructor for an unpinned reader.

**`open_reader` returns `Option`, and `None` means "cannot be read".**
That is the sink contract of §3.2 spelled out in a type rather than left
to a comment, and it is what stops an unreadable store reading as an
empty one.

**And there is finally evidence, in two processes.**
`//datalib/backend/etl:doltlite_two_process_test` (#328) has a real
writer committing into a `.doltlite_db` while a separate process opens
it read-only, pins, and reads `pinned_entities`. It covers both open
orders, requires at least two of the writer's commits to land inside the
reader's sample window so a quiet store cannot pass, and was verified to
fail — counts climbing from 5 to 59 — when the read is pointed at
`entities`. Every single-process test in `pin.rs` was structurally
incapable of answering this, because doltlite's working set and its
chunk-store lock are per *file*.

**The CAS is deliberately unpinned, and that is better than what this
document first suggested.** Entities are committed *after* the blobs they
name, so an entities pin can reference a blob committed later than any
CAS pin a reader sampled, and the pinned CAS would then be missing bytes
the pinned entity points at. Content addressing means there is no version
of a blob to be wrong about. Said once, on `BlobCas::get`.

**Done, as far as an independent sweep can tell.** #333 pinned the four
reads this document named plus a fifth the check's own extension found
(notion `load_user_names`, notion's attachment projection twice, linkedin
`load_photo_blobs`, pdf `scan_root` and `convertible_documents`), and pdf's
`open_reader` made the handle-pin move. A transitive sweep over every
`etl` function reachable from a render file finds nothing left: the two
apparent hits — chatgpt's `load_conversations`, slack's `load_workspace` —
are name collisions with local definitions inside each provider's own
`render/parse.rs`, and the download-side functions of those names are not
called from render.

**And the check now says what it checked.** It prints "7 listed loader(s)
and every render file read a pinned view" rather than "every render read
is pinned", because with a hand-written `_RENDER_REACHABLE_LOADERS` list
the second is exactly what it cannot know. That is the right correction:
the list is still the weak joint, and the honest failure mode is now
stated rather than papered over. The structural answer — put the pin on
the handle so a loader has no way to read unpinned — is what every
provider has now done, which is why the list has stopped growing.

## 4. What a provider supplies

Everything above is generic. A provider on this framework would give
the framework four things and no SQL of its own:

1. **The creating tables** — which upstream tables can bring a new
   document into existence, and how a row there projects to a bucket
   key. (`bucket_query`, narrowed.)
2. **Bucket key → owning conversation uuid(s).** For the scoped retain
   in §3.3 and the deletion rule in §3.1.
3. **Render a bucket** → documents, each carrying its `render_deps`.
4. **Render everything** — the full pass. Most providers already have
   this; it is the cold-start path they take today.

Deletion, the cursor, the pin, the fan-out expansion, the full-pass
fallback and the file sweep all move into the framework.

## 5. Order of work

Each of these is useful alone, and they are listed cheapest-first.

1. **Move the cursor into the store** (§3.4). Mechanical, touches all
   twelve providers shallowly, and is a prerequisite for making the
   full pass atomic.
2. **Make the full pass real** (§3.2) — delete-all + upsert-all + the
   post-commit file sweep. This closes the "cold start deletes nothing"
   hole on its own, before any deps table exists, and subsumes
   `RenderPass`: a pass that cannot read its source is a pass that
   walked nothing, which is the same statement `Skipped` makes today.
3. **Scoped retain per bucket** (§3.3). Small, and fixes the four
   periodizing providers.
4. **`render_deps`** (§3.1). The big one. Land it behind the existing
   behaviour: write the table, and assert (in tests, then in a warning)
   that the documents it names match what `global_fanout_tables` and
   the hand-written unions produce. Only then delete the old paths.
5. ~~**Finish the pin** (§3.5)~~ — done in #333. What remains is the
   residual `prior_fingerprints` plumbing in the five providers still
   threading it under the diff, which is cleanup rather than a hole.
6. **Bring the stragglers on**: beeper (no cursor, no deletions at
   all), sms_backup_restore (deletes after writing), contacts (which
   the deps table unblocks), and the four whole-store providers, whose
   `retain_documents` sweep becomes the framework's full pass.

## 6. What this deliberately does not do

- **Column-level diffs.** Bucket-grained is enough; `render_deps`
  already makes the grain as fine as it usefully gets.
- **Streaming.** Everything here works whether or not render reads a
  finished store, and since #329 claude's download seals mid-fetch, so
  sometimes it does not. Pinning (§3.5) is what makes that safe. The
  one place the two designs meet is §3.2's note on monotonicity: a
  provider that truncates within a run has no safe mid-run seal, and
  the framework's full pass is what a consumer of one should do.
- **The download side.** "What did upstream delete" is a different
  question there, answered by re-enumeration and
  [`prune.rs`](/datalib/backend/etl/src/prune.rs). Unchanged.
- **qmd_index.** The third consumer of the render trees has no cursor
  and walks the filesystem. Bringing it onto the same shape is worth
  doing and is not this.
- **A cold start that is merely slow.** `scan_buckets` used to absorb a
  broken bucket query into a silent cold start; #316 split that from a
  stale cursor, so a query naming a table the store does not have now
  fails. Nothing further is needed here — it is listed so the next
  reader knows the fallback that used to hide bugs is gone.
