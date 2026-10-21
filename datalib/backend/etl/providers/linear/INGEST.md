# Linear: how the download works

The `linear` source copies one Linear workspace through Linear's
GraphQL API (`https://api.linear.app/graphql`). It is download-only:
there is no render step, and the raw store is the copy. Each kind of
Linear object gets its own table of `(id, payload)` rows, keyed by
Linear's id. The payload is the object as the API returned it.

## Signing in

Linear is built into latchkey, and browser sign-in works:

```sh
latchkey auth browser linear
```

Linear reports what this stores as an API key. A personal API key made
in Linear (Settings → Security & access → Personal API keys) works too,
and can be limited to read access:

```sh
latchkey auth set linear -H "Authorization: lin_api_…"
```

Note there is no `Bearer`: Linear takes the bare key.

## What is copied

| table | how it is read | narrowed by the selection |
|---|---|---|
| `teams`, `users`, `workflow_states`, `issue_labels`, `project_statuses`, `initiatives` | whole, every run | no |
| `projects` | whole, every run | the chosen projects, and every project a chosen team works on |
| `cycles` | whole, every run | cycles of a chosen team |
| `project_milestones` | whole, every run | milestones of a kept project |
| `issues` | walked newest first | team or project (see below) |
| `issue_history` | with each issue | follows its issue |
| `comments` | walked newest first | what the comment is on |
| `documents` | walked newest first | team or kept project |
| `project_updates` | walked newest first | kept project |
| `attachments` | walked newest first | its issue's team or project, decided after reading (Linear cannot filter attachments by team) |

`ingest/fields.rs` lists the fields each table asks for. A reference to
another object is asked for as `{ id }` only, so a record changes only
when its own fields change. Some fields are left out on purpose:

- fields whose value depends on who is asking (`favorite`, `isMe`,
  `subscribers`),
- AI summaries that Linear regenerates on its own (`aiAnalysis`,
  `summary`),
- the collaborative-editing state (`descriptionState`, `contentState`,
  `bodyData`), which encodes the same text the markdown field already
  holds.

An archived object is still listed (`includeArchived: true`), with its
`archivedAt` filled in. Nothing is deleted from the store. An object
Linear deletes outright stays as it was last fetched.

## Choosing teams and projects

Leave `teams` and `projects` both empty to copy the whole workspace.
Otherwise an issue is copied when its team is in `teams` **or** its
project is in `projects`. Most issues belong to a team and to no
project, so a list of projects alone leaves most of the workspace out.

- `teams` matches a team by key (`ENG`, ignoring case) or by name.
- `projects` matches a project by its name.
- A name that matches nothing becomes a `config` problem row, and
  selects nothing.

The wizard's pickers load both lists from the live workspace
(`probe.rs`).

## Incremental updates

**The first run copies everything. Every run after it reads only what
changed.** The cost of a later run depends on how much changed, not on
how big the workspace is.

Two kinds of table, two ways of keeping up:

- **The small lists are read whole every run.** Projects and cycles
  carry progress series that Linear recalculates from their issues,
  weekly or whenever one of those issues changes. When that happens,
  the project's own `updatedAt` does not move. On a real workspace,
  projects untouched for 57 days still gained a point of
  `issueCountHistory` every week. So `updatedAt` cannot tell us when
  these changed, and re-reading them is the only way to see it. Re-reading
  them costs about 20 requests. The sidecar is stamped only the first
  time a row is seen (`Stamp::FirstSeen`), so re-reading an unchanged
  row commits nothing.
- **The big connections are walked newest first** (`ingest/walk.rs`).
  Each one is asked for `orderBy: updatedAt`, which Linear returns most
  recent first. Every page is stored in one transaction, together with
  a `coverage` span: the stretch of `updatedAt` stamps the walk has
  read. What is left to walk is every stretch no span covers, newest
  first:
  - A first run walks from now back to the oldest record.
  - Every later run walks from now down to the top of what earlier runs
    covered. On a quiet day that is one page per connection.
  - A run that stopped part-way (a stop, a failed page, a spent budget)
    left a span ending where it stopped. The next run first reads what
    is new since, then finishes the stretch below.

  A record already held at the `updatedAt` it is listed at is not
  written again (`held_version` in its sidecar). An issue's history
  beyond the 50 entries that come with it is fetched only for an issue
  that changed.

Measured on a workspace of 10,551 issues (including archived ones),
12,247 comments and 56,161 history entries:

| run | requests | time | rows written |
|---|---|---|---|
| first | 425 | 5 min 50 s | 27,119, plus the history |
| second, nothing changed | 25 | 13 s | 0; no record table changed |

Linear's budget is 2,500 requests and 3,000,000 complexity points an
hour per user. No single query may cost more than 10,000 points. The
largest page here (50 projects with their members) costs about 2,400.

### Why the walk pages by stamp, not by cursor

Each page asks for records updated at or before the oldest stamp on
the page before it. It does not use Linear's `after` cursor. A record
edited mid-walk moves to the top, since its `updatedAt` only grows. A
bound on the stamp cannot skip a record when that happens, and the
moved record is read by the next run. Both ends of a bound are
inclusive, so the record at a page boundary is listed twice and written
once. The cursor is used only when a whole page shares one stamp, which
would otherwise leave the bound nowhere to go.

### When the selection changes

Coverage is kept per selection: the scope is `<table>:<key>`, where the
key is `all` or a hash of the chosen team and project ids. Choosing
different teams therefore walks the new selection from the top. Records
already held at their version are listed but not rewritten, so the cost
is requests, not writes.

## Volatile fields

A field that changes without anyone editing the record goes into the
sidecar's `volatile_payload` instead of the payload, so a diff of a
table shows real edits only. Each table's list is beside it in
`ingest/schema_raw.rs`:

- `updatedAt` everywhere. A comment, a board drag or a sub-issue moves
  it. On the walked tables it is also the version the record is held at.
- Sort orders (`sortOrder`, `prioritySortOrder`, `subIssueSortOrder`),
  which move when someone drags a neighbour on a board.
- The progress series of projects, cycles, milestones and teams, and
  derived counts (`issueCount`, `customerTicketCount`, `resourceCount`,
  `commentCount`).
- A person's status and `lastSeen`; a project update's `isStale`, which
  turns true with time alone; a comment's AI `threadSummary`.

Id lists Linear returns in no promised order (`labelIds`, and any
`{ nodes: [{ id }] }` connection) are sorted before storing
(`db::canonicalize`).

## Known gaps

- **Comments on documents** are copied only for the whole workspace.
  The narrowed comment filter names issues, project updates and
  projects, not document content.
- **Relations** come with their issue (up to 50). Linear does not
  always move an issue's `updatedAt` when a relation is added, so a new
  relation can wait until the issue next changes.
- **History written after its issue's `updatedAt`.** On a real
  workspace, 2 of 50 issues had a history entry stamped up to a minute
  after the issue itself. History is fetched again only when the
  issue's `updatedAt` moves, so such a late entry can be missed until
  the issue next changes.
- **Uploaded files** (`uploads.linear.app` links in markdown) are not
  copied. They need the credential to download.
