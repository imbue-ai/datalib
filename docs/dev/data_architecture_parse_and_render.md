# Data architecture: parse and render

Third sibling to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md) and
its [practices companion](data_architecture_ingestion_practices.md).
Those two cover the **download** stage — how upstream bytes land on
disk and what shape they have at rest. This one covers what happens
next.

Two words, because they are two things and conflating them causes real
confusion:

- **parse** — deserialize a stored payload into the provider's typed
  in-memory representation. Pure, no I/O; usually `render/parse.rs` in
  the provider's `_render` crate.
- **render** — turn that representation into what the render store
  keeps: a document's markdown, section by section, for people, and its
  `grid_rows` for the index. No `.md` file is written.

Together they are one pipeline stage, the `render_markdown` step of a
source's group. When this doc needs a word for the whole transform it
says **the projection**. What it never says is "the parse step": there
isn't one, and a record that "fails to parse" is a record **render**
could not deserialize — which matters because the fix is always a
re-render and never a re-fetch.

A source whose data is numbers over time renders a second format too:
its `render_time_series` step writes time series, not documents, into a
store of its own, which the `time_series_index` indexes. That path shares
none of this doc's machinery (no markdown, no `grid_rows`, no render
cursor); its writer is `datalib/backend/etl/time_series` and its design
is [`time_series.md`](time_series.md). A calendar's
`render_occurrences` step is the same kind of side path: every
occurrence of its events in a window of days, one row each, in a store
of its own (`calendar_render/src/occurrences/README.md`). Everything
below is about documents.

§2, §3's U-rules and §5 describe the tree as it is. §3's P-rules and
§4's rules are partly aspiration: each says where the tree stands.

## 1. Where the neighbouring material lives

| doc | stage | contains |
| --- | --- | --- |
| [`data_architecture_ingestion.md`](data_architecture_ingestion.md) | download | principles, at-rest shape, operational properties |
| [`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md) | download | testing, adding a provider, schema evolution, open questions |
| **this file** | parse + render | the stage contract, the projection, the parse contract and the unification/fidelity rules, data-quality rules, incrementality and deletion, timestamps |
| [`grid_rows.md`](grid_rows.md) / [`edges.md`](edges.md) | — | the tables render writes into |
| [`entity_ids.md`](entity_ids.md) | — | the `uuid` recipe every projection must follow |

The `datalib-time` crate contract stays in the ingestion doc, since
download stamps its own `fetched_at_utc` with it; §6 here covers only
what render does with a record's own timestamps.

## 2. The stage contract

Render's input is `<data_root>/<name>/ingest/` and **nothing else** — not
the API, not a file-backed source's `input_path`. The full argument is
[Layering of concerns](data_architecture_ingestion.md#layering-of-concerns-download-is-downstream-agnostic);
the one-line version is that the raw store is the boundary between the
outside world and our copy, so everything downstream of it is
reproducible offline.

The consequence that matters for the rest of this document: **every
parse/render defect is fixable by re-rendering.** That is what makes
§4's rules affordable — we can be strict about correctness here
precisely because being wrong here is cheap to correct.

### The render-store contract

After download, we run transformations for display and indexing —
render to markdown with YAML frontmatter, index the documents for
search, derive `grid_rows` for the UI.

The cross-provider contract is the **render store**: one doltlite
database per source, at
`<data_root>/<name>/render_markdown/indexed_markdown.doltlite_db`, holding

  - `markdowns` — one row per rendered document: its `markdown_uuid`,
    its `renderer_version`, the `render_unit_key` it was rendered from, and
    whether the search indexes it (`searchable`). Nothing on it
    is stamped per run — a re-render of an unchanged document writes
    an identical row. Its `front` is the document's front
    matter as its renderer handed it: facts in order
    (`etl/render/src/front_matter.rs`'s `Front`), which one function
    draws as the YAML block the document opens with. Every renderer
    hands its front matter this way and its document as sections, and
    the fixture test refuses a provider that does not.
  - `grid_rows` — the document's projected rows. Each carries, in its
    `supplied_terms` column, what it answers to in search beyond its
    other columns: an email's To, Cc and Bcc by handle, the people a
    message mentions, an email's labels, and on a document's own row who
    took part in it. chat-common and contact-common supply them; the
    index melts them into its search terms, one indexed row per
    term (`etl/render/src/search_terms.rs`).
  - `edges` — its outgoing links: a `Link` the page draws, or a
    `ReplyTo` from an item to the one it answers.
  - `data_nodes` and `filings` — the containers its rows live in, from its
    own up to the top of the source, and what they are filed under
    besides (`docs/dev/grid_rows.md` § "`parent_uuid`, …"). Like edges
    they belong to the document.
  - `sections` — what each piece of the document says, in order, kept
    apart from how it is drawn. An item a renderer hands over as facts
    (chat-common's, `etl/render/src/item.rs`) keeps its body alone and,
    in columns of its own, everything its box shows: who wrote it and
    when, who it was addressed to, its labels and tags, its own link, its
    reactions and its replies count, and whether it continues its
    author's run in a chat; whatever reads it draws the box from
    them with `Item::draw`, as render did.
    Any other piece is kept whole and marked `framed`, so whatever reads
    it draws nothing around it; a piece that is layout alone (front
    matter, a `<details>` opener) the renderer marks as frame, and
    `sections` has no row for it. A renderer that keeps its whole page as one piece keeps
    its front matter at the start of it. The search index draws its text
    from it (`search_index/src/text.rs`): each item as an envelope line
    (who, to whom, when), its labels and its body; a page, a document
    whose first piece is framed, as a line of the facts worth searching
    for from its front matter (`markdowns.front`: the source, the kind,
    the calendar, the repository …), then each framed piece as the words
    a person reads, without its front matter, tags, link targets or
    escapes. The grid index does not copy it
    ([`documents_to_blocks.md`](plans/documents_to_blocks.md)
    §"Store the body, format on read").
  - `drawings` — each frame piece whole: the layout between items.
    With `sections` it spells the document byte for byte
    (`IndexedMarkdownStore::document`), which is the one copy of what a
    document looks like: the viewer and `datalib-step markdown` read it.
    A search's quote reads only the `sections` row of the item the hit
    landed on (`IndexedMarkdownStore::item_texts`).
  - `source_contacts` and `source_contact_handles` — the people it
    describes or mentions, each as its source describes them (a
    `NormalizedContact`, `contact_schema/`), and the handles that reach
    them. chat-common adds one per author handle with no provider code;
    contact-common adds a card's own. Like edges they belong to the
    document, and the index, which keeps each distinct record once
    ([`contacts.md`](contacts.md)), answers "who is this handle" from them
    (`unified_grid_and_search_index`'s `POST /people`).
  - `problems` — what render could not do getting there (§4).
  - `render_inputs` and `render_cursor` — what each render unit was rendered
    from, and how far into the raw store the last run got (§5).
  - `source_measurements` — the storage report's samples
    ([`grid_rows.md`](grid_rows.md)).

The human artifact is markdown with YAML frontmatter, spelled from the
store; `datalib-step markdown` writes it out as `.md` files for whoever
wants them. An attachment its source's blob store holds is linked there, at
`/data/<group>/<function>/blob/<hash>`, which `datalib-http` answers
([`blob_store/README.md`](../../datalib/backend/blob_store/README.md)).
An email's attachments and inline images are MIME parts of the `.eml`
its blob store holds, linked inside it at
`/data/<group>/<function>/blob/<eml hash>/part/<part hash>`. A
person's photo is linked in its source's blob store too
([`contacts.md`](contacts.md) §"Photos"), so nothing a page shows is
written beside it.

Every table's schema is a hand-written struct in `datalib_schema` with
`#[derive(PortableTable)]` deriving the DDL, so the same struct defines
what a renderer writes and what the index reads.

This part of the pipeline aspires to the same properties as download:

  - **Monitorable**: the same progress-bar contract as download, though
    render-side progress reporting is less developed.
  - **Incremental, by diff alone.** Render asks the raw store what
    changed since the commit its cursor names and renders only the
    render units that read those rows; the index asks the render store the
    same question about its own commits. §5 is the whole mechanism.
  - **Resumable at every checkpoint**: render commits the store after
    every batch of documents, and each commit leaves a store a consumer
    can read. A run killed after N of M documents has N of them
    committed; the next run diffs from the cursor it last *sealed*, so
    it re-renders the range once more — to identical rows, which cost
    nothing — and continues.

### Why the projection is a database and not a file tree

The render store is doltlite for the same reason the raw store is: the
question "what changed since my cursor?" is one doltlite answers, and a
file tree of sidecars would have to re-implement it with fingerprints
and full walks. Five properties follow, and each is load-bearing
somewhere downstream:

- **No tree walks.** What a run needs to know about the render store it
  reads through indexed queries; what it needs to know about the raw
  store it reads through `dolt_diff`.
- **A row cannot be unreadable.** A sidecar file could be malformed, and
  the two responses to that (skip it silently; abort the whole load)
  were both bad. There is no file to fail to parse.
- **A document lands whole or not at all.** Its `markdowns` row, grid
  rows, edges and problems are written inside one SQL transaction, so a commit
  landing between two documents — a checkpoint, or the end of the run —
  never publishes a fraction of one.
- **Deletion is expressible.** A `dolt_diff` can name a row that
  *left*, which is how a document a source stops holding reaches the
  grid index as a deletion. Reading whole stores could never see that.
- **The step's output version is free.** The render step reports the
  hash of its store's HEAD tree (`dolt_hashof_db`), which moves only
  when the rows do, so the index skips a render that changed nothing
  ([`dag/README.md`](../../datalib/backend/dag/README.md) says what an
  unreported version costs).

It also simplifies [§4](#4-data-quality-rules)'s problem sink. Render's
rows and the record of what was dropped or nulled getting them there
live in one database with one writer, so they commit in one transaction
and can never disagree about which run they came from. A document whose
every row was dropped is stored as no document at all — no `markdowns`
row, no sections — and its problem rows are the whole record of it.

### The markdown is in the store too

There is no `.md` file. A renderer hands each document over as
`RenderedMarkdown::sections`, and the store keeps them in `sections` and
`drawings`, versioned with the document's rows, so a reader at a
commit sees that commit's text. Search draws a document's text from the
store (`IndexedMarkdownStore::index_docs`); the document view and
`datalib-step markdown` spell it (`IndexedMarkdownStore::document`). A
hit lands on the item the search index names, by its id, and its quote
reads only what that item says (`IndexedMarkdownStore::item_texts`).

The *contract* is this: render emits a human artifact and a
separate machine-readable projection, and the index reads the
projection — **it never re-parses markdown** (AGENTS.md's "Rendered
markdown is never parsed back").

### Upstream text is escaped where it becomes markup

Every way upstream text has gone wrong on its way to the page or to
search is a case in the text-edge library,
[`etl/text_edges/`](../../datalib/backend/etl/text_edges/README.md),
and a change to how text is escaped is checked against it;
`datalib/ui/tests/goldens/text_edges_preview.html` shows each case as
the app draws it.

The UI renders every document with markdown-it and `html: true`
(`ui/src/cards/renderDocument.ts`), because the section wrappers are
HTML. So a name, a subject or a text message written into the markdown
as it is would be read as HTML and markdown: `Bob <bob@x>` loses its
address, and a sender can restyle or fake parts of the page. Scripts
cannot run — the sanitizer drops them and the frame has no
`allow-scripts` — but markup still can.

The rule: **a string that is plain text upstream is escaped at the
point it becomes markup**, by the helper for where it lands —
`datalib_etl_render::html` lists them (between tags, in an attribute,
on one markdown line, a multi-line body, a link target, a code span or
block). **Markup a source authored goes in as written**, HTML
included: a prompt, an assistant's reply or a project's instructions
in Claude, ChatGPT, Codex or Claude Code; a GitHub or GitLab
description, review or comment; a Todoist task, description or
comment; an email's `text/html` part and Google Takeout's Gemini answers and
My Maps descriptions after htmd; a PDF's pages as pdf-inspector wrote
them; a Notion page. Its formatting (`<details>`, `<img>`, `<br>`,
`<kbd>`, a table) is what its author meant, and escaping it would also
turn its code into entities that search cannot read. What keeps a
tag in it from reaching past its own message is the page, not the
renderer: the sanitizer and the frame's policy stop scripts, and each
message is rendered as a fragment of its own (§"One section cannot
reach the next"), so a renderer whose text goes in as written declares
its sections. chat-common closes a fence or an HTML block a message
leaves open (`close_open_blocks`), so the message's own `</div>` is not
read as code. Escaping never drops a character, and `plain_text` (the
Contents cell) reads the escapes back, an escaped `<tag>` as text. A
renderer's test of a plain-text field feeds it `<script>x</script> &
co` and checks it renders escaped.

The trap is text between tags on a markdown line — the message
header's `## <span class="msg-author">…</span>`: markdown-it parses it
as markdown, so it needs `escape_md_inline`, not the HTML-only
`escape_text`. `escape_text` is for text inside an HTML block, and it
writes a line break as `&#10;`, since a blank line would end the block.

`datalib/ui/tests/hostile_text.test.ts` is the check across renderers.
`//datalib/backend/etl/hostile_samples` puts a string of HTML and
markdown (a tag, a link, an image, a `|`, a code span, an entity, a
blank line, a `---` line) into every plain-text field the shared
renderers take — chat-common, calendar-common, contact-common,
forge-render-common, Slack's mrkdwn, `MessageHeader`, `Title` — and
the test renders each document through the app's own markdown-it and
sanitizer and asserts every field reads exactly as typed and makes no
element of its own. It runs the escape helpers the same way over a few
hundred generated strings. A provider's private text paths (Facebook
posts, Claude's tool results, the time-series facts) are not reached
by it and keep their own tests.

**Text is HTML only when its declared type says so**, never because of
where it came from. An email's parts are read by their own
Content-Type: mail-parser's HTML body list follows RFC 8621 and holds a
`text/plain` part that has no HTML alternative, and sending that
through htmd loses every `<…>` in a plain-text patch and runs its lines
together.

**A name a person reads to tell what something is — a title, an author
or recipient, a reactor, a filename, a label — loses its Unicode bidi
controls** (`datalib_etl_render::html::without_bidi_controls`: the
overrides and embeddings U+202A–U+202E and the isolates U+2066–U+2069).
A right-to-left override shows `evil.exe` as `exe.live`. The marks
U+200E, U+200F and U+061C stay: they reorder no letters, and
right-to-left names carry them. A message's body keeps them. The
shared writers apply it for every source:

- on the page: `Title` (every `<h1>`), `message::chip_link` and
  `entity_link` (every chip, its text and its hover), `MessageHeader`'s
  author, `front_matter::yaml_name` (the `title:` key), and
  `html::escape_name_inline` / `escape_name_text` for a name with no
  handle, a filename or a label, which each renderer calls where it
  writes one (calendar-common, contact-common, forge-render-common,
  Notion, Todoist, Google Drive, PDF, Lightroom; chat-common strips
  the bidi controls from its chat's and items' names first);
- in the store: `grid_index::apply_page` (`apply_one` is a page of one),
  which every write of a document into a render store or the grid index
  goes through
  (`IndexedMarkdownStore::put_document` included), takes it through
  `names::settled`. That strips the rows' kind, source label, author,
  contact, email, phone, account, project, org, channel, subject and
  supplied search terms; each edge's label; each source contact's
  names, org, title, groups and members; and each photo's title,
  headline, copyright, creator, camera, lens, color label, keywords,
  people, albums, place names, file name and folder. A row's preview
  and a photo's caption are prose and keep their controls. A
  `GridRow` string column either has its bidi controls stripped there
  or is listed as an id, path, stamp or body in `every_grid_row_string_is_settled_or_kept`,
  which fails on a new column that is in neither list.

A change to what these write bumps
`datalib_etl_render::SHARED_RULES_VERSION`, which is part of every
source's render params, so every source renders again; a provider
bumps its own version only for its own change.

Front matter is YAML, and a value from upstream goes through
`datalib_etl_render::front_matter::yaml_scalar`, which JSON-quotes it,
so a line break, a quote or a `---` inside cannot end the line or the
block. The applet strips front matter at the first line that is `---`
by itself.

### One section cannot reach the next

Escaping covers only text that was plain upstream: markup a source
authored goes in as it is, and a renderer can miss a field. Rendered as
one string, a tag one message leaves open would run
on through every message after it: an unclosed
`<div style="display:none">` would hide the rest of the conversation,
and a copied `<div id="m-…" data-section-uuid="…" class="msg">` could
pose as a later message, so that a click on that message's grid row
landed on the copy. Markdown has no "close everything here" marker.

So **the UI renders and sanitizes each message as a fragment of its
own.** The HTML parser closes whatever a fragment leaves open at the
fragment's end, so nothing in one message can wrap or replace the next.

**Where the messages begin comes from the projection, never from the
text.** Upstream text can contain any bytes, the wrapper's included,
but it cannot move where one piece ends and the next begins. A renderer
hands its document over as `Section`s (`etl/render/src/section.rs`):
keyed pieces, one per message, each keyed by its `data-section-uuid`,
and unkeyed ones (the front matter and title, the `<details>` opener
and closer around a run of tool steps). Joined in order they are the
document. A message is drawn in its **box**, `<div id="m-{uuid}"
data-section-uuid="{uuid}" class="…">`, and every renderer draws it
one way, `Section::boxed`, so the store keeps the box's class
(`sections.class`) and knows the box from the uuid and the class alone.
A box that is a part of a page rather than a message on it (a PDF's
page, a device on a time-series page) also carries `msg--page`
(`section::PAGE_PART`), and the UI draws it flat and whole: no card,
no "Show more".
A **fold**, a `<details>` around a run of pieces (a run of tool steps,
another version, a Drive comment thread, Todoist's completed tasks), is
drawn one way too, by `Section::fold_open` and `Section::fold_close`,
and the store keeps each one's class and summary in `folds`. The render
store keeps the pieces, and spelling a document back gives each piece's
uuid or none, its box's class, the fold it opens or closes, and its
text; the document route, asked with `?pieces=true`, sends them as
`pieces` (`section::nodes`): a boxed one as its class and what is inside
the box, a fold as a node around the pieces it holds. The UI never sees
them joined.

How the UI draws the pieces (`ui/src/cards/renderDocument.ts`):

- **Every piece is rendered alone**, the unkeyed ones (the title,
  headings between messages) too, so nothing one opens reaches the
  next. A fold is drawn by the UI around its pieces, its summary
  sanitized like any other text, so no piece opens or closes it.
- **Each keyed piece is rendered and sanitized alone.** The UI draws a
  boxed one's box itself, around what is inside it rendered as a
  fragment of its own, so a `</div>` in a message cannot close its box
  early either.
- **Only the box answers to a message's id.** What answers to an id is
  the outermost element naming it (`sectionEl` in `chatSections.js`),
  and every lookup by id goes through it: a selection, an edge, a
  problem's jump, the 🔗 button, the right-click menu. A copy of another
  message's wrapper inside a message sits in that message's box, so it
  is left as written and never found first. A keyed piece in no box
  answers by its own wrapper, where it draws one.
- **A message draws only inside its own box.** `documentBody.css`
  gives every box `contain: paint`, so `position: fixed`, a negative
  margin or a transform in one message cannot cover another.

A renderer that keys none of its sections hands its page over as
unkeyed pieces, drawn as one fragment; its boxes still get
`contain: paint`. `grep -rlnE 'Section::(keyed|boxed)'
datalib/backend/etl` lists the renderers that key theirs. Teaching
another one is keying its `RenderedMarkdown::sections`, and the next
render stores them.

The cost is a sanitizer pass per message: in the unit tests' jsdom, a
2,000-message conversation took about 0.7 s piece by piece against
0.4 s whole. `ui/tests/section_isolation.test.ts` holds the rule, and
`ui/tests/e2e/section-isolation.spec.ts` checks it in a browser on a
real document, with a control that shows the same text does hide the
next message once its pieces are joined into one.

## 3. The projection

Three shapes, in order:

1. **The stored payload** — JSONB, wire-fidelity, whatever upstream
   sent. Owned by download, described in `schema_raw.rs`.
2. **The typed POD** — the provider's own typed in-memory
   representation, in `render/parse.rs`. Where a shape is shared
   across providers the canonical type lives in a shared crate:
   `chat-common`'s `Normalized*` types for the two chat families,
   `contact-common`'s for contacts.
3. **The rows** — `GridRow` + `EdgeRow`, handed back through
   `ctx.emit_doc` as a `RenderedMarkdown` and written into the
   source's render store.

(1) → (2) is parse; (2) → (3) plus the markdown is render. Both are
pure given the raw store, and both are the right place for §4's tests.

### Identity and backpointers are first-class in the projection

Backpointers and outlinks are first-class in the projection schema.
`GridRow` carries:

- `uuid` — the Ship-of-Theseus identity, deterministic from upstream so
  re-rendering is idempotent.
- `upstream_id`, `upstream_entity_kind`, `upstream_account` — what the
  uuid was minted from: the provider's own key, its word for the kind
  of record, and the account the record names, so a row can be taken
  back to the provider's API.
- `source_url` — the canonical URL on the provider's web UI (e.g.
  `pull_request.html_url`), populated everywhere we can construct it.
- `markdown_uuid` — the document the row belongs to, whose markdown
  is kept in its source's render store under that id, and nowhere
  else. `source_id` is filled when the row is written, not by the
  renderer ([`grid_rows.md`](grid_rows.md) §"`source_id`").
- Provider-specific cross-references (`notion_page_uuid`,
  `notion_block_uuid`, `git_sha`, …) so the UI can link sideways as well
  as out.

The `uuid` recipe is [`entity_ids.md`](entity_ids.md) and it is not
optional: anything durably keyed on a row — a link handed out today,
annotations and labels later — rests on it staying stable across a
re-render. A content-hash identity would orphan every such reference
on the first edit.

### Unified where possible, per-provider where not

When several sources are shaped similarly enough (a matter of taste,
but largely driven by schema and UI overlap), they should be massaged
into a **shared canonical schema** so the rest of the pipeline (search,
display, threading, attachments, exports) shares code paths and stays
consistent.

Where unification happens: the `GridRow` projection
(the hand-written struct at
[`datalib/backend/schema/src/grid_rows.rs`](../../datalib/backend/schema/src/grid_rows.rs),
whose DDL is derived via `#[derive(PortableTable)]` — see
[`grid_rows.md`](grid_rows.md)).
Every searchable entity from every provider collapses into rows of one
schema with `provider` + `kind` discriminators. The grid backend
reads it with a single query and renders it without knowing which
provider produced any given row.

Unification should **never** happen in the raw store: Slack, Beeper,
Signal, Claude, and ChatGPT each have their own raw tables, in their own
doltlite DBs — and because each store is its own file, the tables need
no provider prefix to stay apart. Slack's messages are in `messages`,
Beeper's in `mx_room_messages`, Signal's in `chat_items`. The full list is the
`schema_inventory` golden. Once we
*render*, though, we aspire to share as much as possible — projecting
raw data into unified schemas where appropriate, then sending that
unified data through common code paths for interpretation, rendering,
and indexing.

Examples where schema and data handling should be unified:

  1. **Chat (human)** — Slack, Beeper, Signal, WhatsApp, Apple
     Messages and the rest. "Messages in channels/DMs between humans
     with attachments and threading." Per-provider raw and parse, one
     render through `chat-common`.
  2. **Chat (LLM)** — Claude, ChatGPT, Claude Code, Codex. Same chat
     shape but with assistant turns, thinking, and tool use surfaced,
     told apart by `kind` (`User Input`, `LLM Response`, `LLM Thinking`,
     `Tool Call`, …).
  3. **Code review threads** — GitHub PR discussions, GitLab MR
     discussions. Threaded inline comments on diffs, rendered through
     `forge-render-common`; `git_sha` is there to serve this family.
  4. **Document-comment threads** — Notion. Very similar in shape to
     (3), and shares only the `GridRow` projection with it.
  5. **Time-series sensor data** — yolink, airvisual, garmin.
     Per-device samples over time with a small fixed set of value
     channels. Each provider keeps its own raw schema, parse and metric
     table; the sensor page, the plot and the rows (one `Sensor
     Timeseries` row for the page plus a `Sensor Device` row per
     device, `timeseries_render/src/page.rs`) are shared in
     `timeseries_render`. Garmin uses the plot and text helpers but
     writes its own documents: the weight page, a page per family of
     daily metrics, a daily log, one page per activity, and its
     records (`garmin_render/TRANSLATE.md`).

A new provider that fits a family should at minimum project to the
family's `GridRow` shape rather than inventing a new `kind` taxonomy.
A provider that doesn't fit may motivate a new family; opening one
should be deliberate.

### What parse owes the rest of the pipeline

Parse is the smaller half of the projection and the easier one to get
wrong quietly, because its output is consumed only by our own render
code — there is no schema check between them, and a field that comes
out subtly wrong looks exactly like a field that came out right.

Five rules. P1–P3 are contract; P4 and P5 are about where work belongs.

**P1 — Parse is total.** A record that will not deserialize is *that
record's* problem, not the step's. Parse reports what it could not read
and keeps going; only a systemically wrong input (§4's R2 third
category) may fail the step. Concretely this means the parse of a
collection returns the records it got **and** the problems it hit, not
one or the other — see R1's sink. The parses of calendar, chatgpt,
email, garmin and slack do this (`Unparsed`, reported through
`RenderCtx::report_unparsed`).

**P2 — Declare the type you expect, and what happens when it isn't
that.** Every field parse reads has a declared coercion. A value the
declaration does not cover is nulled and reported, never passed through
untyped and never guessed at. "This field is a string, except the three
times upstream sent a list" is a thing to record, not a thing to
paper over.

**P3 — A value with cross-source meaning is parsed by a shared crate,
never by a provider.** This is the rule that makes unification possible
at all, and it is the one most worth stating, because breaking it is
invisible: a provider that hand-rolls its own timestamp parser produces
values that *look* fine and are quietly incomparable with every other
provider's.

If a concept means the same thing across sources, exactly one crate
owns reading and writing it, and every provider calls that crate:

| concept | the one owner |
| --- | --- |
| timestamps | [`datalib-time`](../../datalib/backend/time/src/lib.rs) — `parse_strict`, `parse_with_assumed_utc`, `parse_custom_strftime`, `bump_micros` |
| entity identity | [`datalib-id`](../../datalib/backend/id/src/lib.rs) — the five-component v5 recipe in [`entity_ids.md`](entity_ids.md) |
| the chat shape | `chat-common`'s `Normalized*` types |
| the contact shape | `contact-common`'s |

The list is short because the set of genuinely cross-source concepts is
small. It should grow deliberately: the test for admitting one is
whether two providers disagreeing about it would produce a *wrong
answer* rather than merely an inconsistent-looking one.

**P4 — Parse reads the raw store and nothing else** (§2). Parse is
where the temptation appears: a file-backed provider's `input_path` is
*download's* input, not parse's.

**P5 — Parse produces the provider's shape; render produces the shared
one.** The seam matters. `render/parse.rs` deserializes into types that
look like *that provider's* data, with its own vocabulary and its own
optionality. Projecting onto a shared schema is render's job. Keeping
the two apart is what lets a provider's oddities stay in one file
instead of leaking into a type fifteen providers depend on — and it is
why the unification rules below are all about render.

### Unification and fidelity, and how the tension resolves

Unification is a stated goal of this project: one `grid_rows` schema,
one query behind the grid, `before:` and `after:` meaning the same
thing whether the row came from Slack or GitHub or Notion. Without it
there is no union grid — only a view per provider.

But every unification is a **claim that two things from different
sources are the same kind of thing**, and that claim is lossy at the
edges and sometimes simply wrong. A Slack `ts` and a Notion
`last_edited_time` are both "when," but one is when a human pressed
enter and the other is when anything on a page last moved. Collapsing
them into one sortable column is useful and is not free.

The tension does not resolve by choosing. It resolves by **layering**,
and the layering is already the architecture:

- **The raw store is where fidelity lives.** Wire-faithful, never
  unified (§3 above says this about the raw store, and
  [the ingestion doc](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)
  argues it in full).
- **The projection is where unification lives.** And because the raw
  store is complete and re-rendering is cheap, *the projection can
  afford to be aggressive* — a unification that turns out to be wrong
  costs a re-render, not a re-fetch. That is the same affordability
  argument §2 makes for §4's rules, applied to schema design.

So the question at any given field is never "unify or preserve." It is
**"can the consumer tell?"** — because the raw store means nothing is
truly lost, and the only real harm is a unified value sitting in a
column a consumer reads as if it were what upstream said.

Five rules follow.

**U1 — Unify the frame, keep the value.** Normalize *representation*
freely; normalize *information* almost never. Timestamps are the worked
example and the one to reason from: we unify the format completely —
ISO-8601, explicit offset, one column, one parser — and refuse to unify
the value, because [§6](#6-timestamps) keeps the source's own UTC
offset rather than normalizing to UTC. Format unification costs
nothing and buys everything; value normalization is where information
is lost.

**U2 — When a value must be unified to be comparable, derive it
*beside* the faithful one, never over it.** This is the load-bearing
rule, and the tree already follows it in four places:

| faithful column | unified companion | what the unification buys |
| --- | --- | --- |
| `created_at` — source offset preserved verbatim | `created_at_utc` + `created_offset`, derived at load by `split_record_stamp` | one zone and one width, so lexical order *is* chronological order |
| `upstream_id` + `upstream_entity_kind` + `upstream_account` — the provider's own identity, byte-exact | `uuid` — our v5 over the five-component recipe | one id space across every provider; stable across re-render |
| `upstream_entity_kind` — the upstream's own word, which "may not [be reworded], because `uuid` derives from it" | `kind` — the grid's display label, which "may be reworded freely" | one Kind column the UI can filter on |
| `blake3` — the whole file | `payload_blake3` — the metadata-excluding digest | "same audio, different tags" becomes a query |

Read that table as one idea stated four times. A consumer that wants
comparison reads the derived column; a consumer that wants to know what
upstream actually said reads the faithful one; and **nobody has to make
that choice on everyone else's behalf.** It is [R6](#r6--findings-are-for-the-consumer-not-fixes-for-the-projection)
expressed as schema rather than as advice.

The failure this prevents is specific: overwriting the faithful column
is irreversible *from the index*, and while the raw store can still
answer, every consumer downstream of the index has silently lost the
distinction and cannot tell that it did.

**U3 — Stamp the recipe beside a derived value when more than one
recipe is possible.** `media` does this and explains why in one
sentence: two payload hashes "are only comparable under one recipe, so
the recipe is stored beside the digest" — hence `payload_scheme`
(`mp3.frames.v1`), and any change to what a recipe excludes bumps its
version. `upstream_account` is the same move for identity: the exact
string a `uuid` was minted under, kept so the id can be regenerated and
checked.

The rule has a real limit, and knowing it stops the pattern from
becoming ritual: `created_at_utc` carries no scheme column and should not,
because there is only ever one way to render an instant in UTC. Stamp
the recipe when a *choice* was made, not merely when a derivation
happened.

**U4 — Collapsing a taxonomy is allowed; losing the upstream term is
not.** Render may map a provider's twenty event types onto three
buckets when three is what the layout needs — `chat-common`'s
`ItemKind` (Text / Attachment / System) does exactly that, and it is
the right call for a renderer that has to lay something out. What makes
it safe is that the upstream's own word survives in
`upstream_entity_kind`. Collapse for the consumer; keep the original
for the record.

**U5 — Unify at render, never at parse (P5), and never in the raw
store.** A provider that finds itself unifying inside `parse.rs` is
usually about to teach a shared type something only it knows.

**When not to unify at all.** A shape that does not fit an existing
family should not be forced into it. The tell is a provider inventing
`kind` values that mean something different from every other member's,
or needing a column no sibling would ever set. Two families are
cheaper than one family with an exception in it.

## 4. Data-quality rules

R1's sink is built for every stage — fetch, parse, render and grid row
— and on screen. R2's middle category is followed where the sink is
wired. R3–R7 are adopted in principle and not built. The sink is the
`problems` table (`datalib_problems`), one row per problem per record:
a fetch problem in the source's raw store, everything else in its
render store. Each is read where it is kept, counted on the Manage row
and shown on the document. How it is wired at each stage:

| stage | how a problem gets in | swept by |
| --- | --- | --- |
| fetch, one record | `record_object_attempt`'s failure arm (`record_object_error`), in the raw store, `Reason::FetchFailed` | the next attempt on that record, success or failure |
| fetch, a configured entry upstream does not have | `RunProblems::config`, in the raw store, keyed `config:<setting>:<value>` | the next run that looked every entry up, whose set replaces the last one's whole |
| fetch, a listing or phase the run could not do | `RunProblems::listing` / `phase`, in the raw store, keyed `listing:<name>` / `phase:<name>` | the next run that reached every listing and phase; one that was stopped or cut short clears none (`data_architecture_ingestion.md` §"Error handling") |
| grid row | `GridRowBuilder::build_or_record` | the document, when re-rendered |
| parse, in a document | `NormalizedChatItem::problems` (`own_stamp_ms` for a stamp) | the document |
| parse, no document yet | `RenderCtx::report_unparsed` with a `ReadScope` | the tables the parse read whole |
| render, whole document | `RenderCtx::report_document_failed` | the document, when it next renders |

The fetch-stage rows are the download side's; the rule for writing
one is in [`data_architecture_ingestion.md` §"Error handling"](data_architecture_ingestion.md#error-handling),
and where they are read is [`etl/README.md` §"Problems stay in the step that found them"](/datalib/backend/etl/README.md#problems-stay-in-the-step-that-found-them).

Several of the rules below are phrased after the `data-pipeline-builder`
skill in
[`imbue-ai/default-workspace-template#534`](https://github.com/imbue-ai/default-workspace-template/pull/534).

### R1 — Drop, count, log; never abort, never hide

Every problem goes through one sink,
and the sink has a taxonomy first — what was lost — and a severity
second, derived from it unless the writer says otherwise:

| what happened | what we do |
| --- | --- |
| unreadable or undeserializable document | drop the record |
| no usable identity | drop the record |
| a field fails its declared coercion | null **that field**, keep the record |
| a value whose type the contract does not cover | null that field — never pass it through untyped |

`GridRowBuilder::build_or_record` is this sink for the grid-row stage:
a `created_at` that will not parse is nulled and the row kept, a row with
no identity is dropped, and each lands as a `problems` row with a
deterministic id, swept when its document is next rendered.

Every one emits `{source, stage, key_or_path, field, reason, sample}`,
where `sample` is the first 80 characters. Never a count without a
reason, never a reason without a sample. The test of the design: **run
once, read the log, fix the projection for every reason it lists,
re-render.** If reading the log doesn't tell you
what to change, the sink is wrong.

### R2 — Three failure categories, not two

[`step_protocol.md`](step_protocol.md) draws one line — absent vs
malformed — and classifies a record that will not deserialize as a
`data` failure, which fails the step and poisons its entire downstream
subtree, including the `unified_grid_and_search_index` fan-in that depends on *every*
source. Right for "the store will not open," wrong for "one of forty
thousand Slack messages has a field we did not expect."

- **absent** — nothing to render: emit nothing, exit 0.
- **malformed but isolated** — this record is bad, the rest are fine:
  drop it, count it (R1), continue, exit 0. A provider's parse
  collects the rows it could not read as `Unparsed` instead of
  `continue`ing past them, and its processor reports them; a
  conversion that fails on one document reports that document.
- **malformed systemically** — the input is not what we think it is:
  exit non-zero, `data`, poison the subtree. The threshold between
  this and the previous category is R4, which is not built.

### R3 — Any rule that turns a non-null source value into null is a judgment call

It follows that every such rule gets a row
in a per-provider table: the rule, the contract line that justifies it,
and **the number of records it affected on the last run**, generated
from R1's counts rather than hand-maintained. If we cannot generate the
count, the rule is not allowed.

This is the render-stage sibling of
[No fabricated timestamps](#no-fabricated-timestamps),
generalized: that section says don't invent a value when upstream is
silent, and this one says when you *discard* one, say so where a human
will see it.

### R4 — A run that drops too much stops

The line between R2's second and third category is a number. Start at
"more than 20% of the records read in this step were dropped": print
the log path, exit non-zero. Deliberately cruder than shape detection,
and worth having precisely because
[Detecting upstream shape drift](data_architecture_ingestion_practices.md#detecting-upstream-shape-drift)
is not built and we don't know what we want there.

### R5 — Verify against the source, not against yesterday's output

Every provider needs at least one assertion comparing a rendered row
back to the raw payload it came from, **sharing no code with the
projection**. Goldens stay — they are good at catching *unintended*
change — but a golden asserts that output matches what it matched last
time, so a field dropped the day a provider landed passes forever.

The failure mode this catches: a projection bug **corrupts attribution
while every count still looks right.** Row counts, provider coverage, uuid uniqueness — all the
things our fixture test already checks — are exactly the signals that
stay green when a field is silently wrong.

### R6 — Findings are for the consumer, not fixes for the projection

Store and emit raw values;
**grouping normalization, axis clamping and null bucketing belong to
the layer that displays the data**, not to the projection. When
profiling turns up that a group-by field has forty spelling variants
that collapse to twelve under case-folding, that is a *finding to
publish*, not a normalization to apply — because applying it destroys
the distinction and the consumer can never get it back.

This is the same instinct as
[Wire-fidelity of the raw store](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)
moved one stage later: download preserves what upstream said, render
preserves what the projection found, and no layer reshapes its data
for the one after it.

### R7 — Bound what grows, and say so where it shows

Everything render appends to grows across runs: `render_markdown/`, the
render store, the problem log from R1. Give each a bound or an explicit
"unbounded, because X" in its module doc — and enforce it **inside the
run that writes, never as a separate cleanup chore someone must
remember.** Where a bound exists, the status surface states it, so
limits are never secret.

Partly blocked: deleting rows from a `.doltlite_db` and committing
reclaims no disk, because earlier commits still reach them; only
rewriting that history and then running `dolt_gc()` does
([disk space](doltlite.md#disk-space-and-dolt_gc), and see
[Removing a source](data_architecture_ingestion_practices.md#removing-a-source)).
The *stating* half is not blocked.

**A render store's history is given back on request**, by its step's
`--compact` ([`step_protocol.md`](step_protocol.md)), never by a run on
its own. The raw store's history is kept: it is the copy we cannot
re-fetch. Documents a source no longer produces are
not part of the problem — §5's sweep removes them in the run that stops producing them; only the store's *history* of
them stays.

## 5. Incrementality and deletion

Incremental render answers one question: *given the raw rows that
changed since the last render, which documents need rendering again?*
Deletion is the other half of the same question — a document whose
inputs are gone re-renders to nothing. Both are answered from one
record, and the render step driver never infers a deletion from a
document's *absence*: absence has three meanings (gone, not looked at,
could not look), and reading it as "gone" deletes live data. The rule
the whole section rests on:

> **`dolt_diff` between two commits of our own raw store reports every
> row that left. Render never infers a deletion from anything else.**

### What is recorded

Two tables in the render store, both written by the driver
(`datalib_step/src/render.rs`):

- **`render_cursor`** — one row: the raw store's commit the last run
  consumed, and the render params (a label filter, a size cap — whatever
  each processor declares through `RenderProcessor::render_params`)
  the documents were rendered with. Beside the processors' own, the
  driver adds two of every source's: the render store's DDL hash and
  `datalib_handle::RULES_VERSION`, so a new `grid_rows` column or a
  change to what a handle normalizes to renders every source again
  without anyone bumping a `RENDER_VERSION`. The driver writes it in the
  same transaction as the run's last work, so it can never claim a range
  the store's rows do not reflect, and rewrites it only when it moves.
  A run that rendered nothing over a range where only bookkeeping moved
  (`sync_runs`, the sidecars: every download commits one) leaves it
  where it was, so the store's version stays put and nothing downstream
  runs; the next diff from it reads the same.
- **`render_inputs`** — `(render_unit_key, input_table, input_id)`, one row
  per raw row a render unit's render **asked for, found or not**. A thread
  rendered while its author's `users` row had not been fetched yet
  records `(users, U123)` anyway, so the row's arrival names the
  thread. Keyed on the render unit rather than the document because one
  conversation can render as several documents (a session and each of
  its sub-agents' transcripts), and the inputs belong to the
  conversation.
  `markdowns.render_unit_key` is the link back: the render unit each document
  came from.

A **render unit** is what a provider loads and renders together, and
what the render decides to redo when a download row changes: a
conversation, a thread, a PR, a page. Almost every render unit makes
exactly one document, the thing a person reads together. The
exceptions render a whole source as one unit — Garmin, AirVisual,
YoLink, and Perseus with its whole file tree — and may write several
documents from it. A provider declares each render unit it rendered, once, through
`RenderCtx::declare_render_unit(render_unit_key, inputs)`
(`etl/render/src/processor.rs`), and every document it emits carries
that key. The declaring is done where the rows are read:
`datalib_etl_render::inputs::Inputs` collects them as a render unit is
built, and `Lookup` wraps a lookup table (users, channels, recipients)
so a key cannot be read without being declared, a miss included. A
processor that reads a table whole declares `Input::whole_table`
(`input_id = '*'`), which any row of the table matches. A composite key
is one string, its columns in `pragma_table_info` order joined by `|`.

A row a provider reads but does not declare will not re-render the
render unit when that row changes, and nothing else will notice.
`tests/fixtures/render_contract_test.py` is the check: for every raw
table in the TNG fixture it edits one row, runs the incremental path,
and asserts that the result equals a cold render and that exactly the
render units the store says read that row were rendered.

### How a run decides what to render

The driver reads the stored cursor and decides a plan. A renderer
version bump (the tree holds `markdowns.renderer_version` values the
processors no longer declare) or a params change means **everything**;
otherwise it diffs from the stored commit; with no cursor at all the
provider reads its whole store, which is not the same as "everything"
— see the sweep below.

With a cursor, a raw doltlite store and something declared, the driver
runs the **reverse lookup** (`reverse_lookup`): it pins the raw store's
HEAD, asks `dolt_diff` for every changed key in every table
`render_inputs` mentions between the cursor and the pin, and reads back
the render units that declared any of those rows. A table that held rows at
the cursor and holds none at the pin — every table of a store `--reset`
emptied — is answered without its diff, which would read every row that
left: for each render unit that named rows of it, the render unit's keys are
looked up at the cursor until one is found (a primary-key seek in
`dolt_at_<table>`). That row left, so the render unit is stale and has lost
a row; a render unit that read the table whole is stale too. A render unit that
only ever looked up keys the table did not have read nothing that
changed, and telling it apart needs the diff, so then the table is
diffed after all, as it is when a key cannot be sought exactly. The
answer is the diff's either way. The provider is handed the
pin and that stale set (`RenderCtx::raw_pin`, `RenderCtx::stale_render_units`,
together `RawRange`), reads at the pin and never at HEAD, so what it
loads is what the set was computed against. A table the driver cannot
diff — the file was replaced by hand, a schema change dropped the table
— is logged and the scan is left to the provider.

The provider adds its own **forward** scan, because a row that *arrived*
was nobody's input yet: a `scan_render_units` query over the added
and modified rows of its primary tables, or `changed_rows` mapped
through the rows it loads anyway where the key lives inside the payload.
`RawRange::narrow` joins the two; `narrow_by` also hands back the stale
keys whose raw row no longer exists, for the processor to declare with
nothing. `fanout_query` — a change that re-renders the whole source —
is unset everywhere but email under a label filter, where a mailbox
added, removed, renamed or moved can change which threads render at
all. It names those columns, not the table: a mailbox row that changed
only in a key the filter does not read must not re-render every thread.

**Every emitted document is written.** There is no fingerprint deciding
whether a write is needed: a row identical to the stored one is no
change to doltlite, carries no diff, and is never seen by the index
([diffs](doltlite.md#diffs)). A fingerprint on top of that is a second
answer to a question the store already answers, and a worse one — an
input hash that leaves out a value resolved at render time skips
changed documents, and an output hash puts a per-run column on
`markdowns`. The rule instead, pinned by
`markdowns_carries_no_per_run_stamp` in `datalib_schema`: **nothing
per-run may be written into a row whose content did not change.** The
storage report is the one document the driver decides about itself,
because its byte counts change from run to run: it is rewritten only when a
row *count* moved (`introspect::counts_unchanged`).

### The sweep

Deletion happens at the end of a run that got through every processor,
in the same transaction as the storage report and the cursor
(`seal_run`), and it has two halves:

- **Per render unit, every run.** A processor ends each render unit it looked at
  one of three ways (`RenderCtx`, `etl/render/src/processor.rs`), and
  the driver decides what that does to the documents the store holds
  under its `render_unit_key` (`fate` in `datalib_step/src/render.rs`):

  | the processor said | the render unit's documents |
  |---|---|
  | `declare_render_unit` with the rows it read | replaced by what it emitted; any other document under the key is removed (a render unit that re-rendered to fewer documents drops the extra ones) |
  | `declare_render_unit` with no rows, and emitted nothing | removed **only when its rows left it**: a row the render unit was last built from (its `render_inputs` as they stood before the run) left — the raw diff reports it `removed`, or its table, emptied since, held it at the cursor — or every one of those rows is now read by a render unit that is new this run (it had no `render_inputs` before) — the key the render unit is minted from changed (a renamed address book re-keys its cards), and its rows build that render unit instead. A render unit that already read those rows is no evidence: a calendar series reads every row of its changed occurrences, and a Claude project page's rows are all read by its conversations. Otherwise they stay as they were, each with a `no_document` warning, and the render unit keeps its old `render_inputs` |
  | `exclude_render_unit` — left out on purpose (email's label filter), or known gone (a calendar reads every event row each run, so a key no event mints is gone though its row is still there: a "this and following" edit moves an occurrence to a new UID) | removed, on the renderer's word |
  | `fail_render_unit(why)` — its build failed | kept, each with a `render_failed` error carrying `why`, even on a full walk |

  The warning and the error are scoped to the document, so they show
  on its banner, and they clear when the render unit next renders it or
  removes it. A whole-table input is never the evidence: one row of a
  table leaving, or another render unit reading it, says nothing about a
  render unit that read all of it. The diff runs from the stored cursor on
  every run, a full walk included, so a version bump keeps an
  unexplained render unit's documents too. A run with no diff to read — no
  cursor yet, or a range the store cannot resolve — has no evidence,
  so it removes nothing this way. And a
  render unit the run never looked at says nothing about its documents, so
  a narrowed run cannot delete its own steady state.
- **Whole store, on a full walk only.** A *full walk* is a run that
  rendered everything (version or params changed) **and** in which
  every processor reported the raw commit it read
  (`RenderCtx::consumed`). Then a document the walk did not produce —
  a uuid minted by the old recipe — is gone, except the documents of a render unit the table above
  keeps. A first run with no cursor is not a full walk, and
  neither is one in which a processor read no store (none on disk,
  nothing committed): it said nothing about what should exist, and
  nothing is swept.

A kept document still carries the render version that last built it.
The version checks (`IndexedMarkdownStore::render_versions`) leave out
a document with a render-stage `render_failed` or `no_document`
problem, so one render unit that will not build neither fails the step nor
makes every later run a full walk.

A removed document goes from the store whole, its text with its rows,
so nothing of it is left for `/applet/unified_grid_and_search_index/chat/{uuid}`
or an export to show. `unified_grid_and_search_index` needs no part in
this — it learns of a removal from the render store's own diff.

Across a version bump or a params change the store is **kept**: its
history, the commit `unified_grid_and_search_index` last consumed, and the cursor. "Render
every render unit again" and "forget where I was" are different requests,
and only the first is ever wanted — a re-keyed document then reaches the
index as a deletion plus an addition rather than as an old row nobody
removes.

### Edge cases

1. **First run, no cursor.** The provider reads its whole store at
   HEAD; nothing is swept except under the render units it built; the
   cursor is recorded at the end.

2. **A reset of the raw store.** `datalib-pipeline reset` empties the
   raw tables in a commit of their own and the next sync refills them;
   the render cursor stays, so the next diff runs from the old commit
   to the pin, where the tables are populated again, and an unchanged
   row diffs as unchanged. A render that ran *between* the two would
   see every row as removed, and that is the truth of the store at that
   moment; it cannot happen by accident, because render only runs
   downstream of its ingest. A reset of the render store itself
   (`--reset <source>/render_markdown`) empties its tables, the
   `render_cursor` table among them, and its documents, so the next
   render is a first run.

3. **A store with tables but no commit.** Download ran but nothing
   was committed — a test that bypasses the runner. `pin::head`
   refuses to pin it ("unreadable, not empty"), the provider reads
   nothing and reports no consumed commit, the cursor stays and
   nothing is swept. Have the test commit after download. Two
   neighbours look similar and are not: a cursor the store cannot
   resolve (someone replaced the file by hand) is a `warn` every run
   and a render of everything, and a `render_unit_query` naming a table or
   column the store does not have is an **error**, not a cold start —
   cold-starting past it would re-render everything on every run and
   never say why.

4. **Removed rows.** The diff names them, the reverse lookup names the
   render units that read them, and each renders to fewer documents or to
   none and is declared so. The per-render-unit sweep removes the rest —
   nothing sits waiting for a collector.

5. **Multi-commit ranges.** Five ingest runs between two renders are
   one diff from the cursor to the pin; a row that left and came back
   in between is `unchanged` or `modified`, which is the right answer.

6. **Concurrent renders.** Two render steps on one source are two
   writers on one doltlite file, which AGENTS.md's "Doltlite: one
   writer per file" rules out; the runner never schedules it.

7. **No doltlite extension.** A build without `dolt_hashof` reads the
   same as a store with no commit to pin (case 3): nothing to read,
   cursor untouched.

### Incremental equals cold: the property the tests hold

Everything above — the cursor, the diff, the reverse lookup, the sweep,
the checkpoints — exists so that a run does less work than rendering
from scratch, and it is correct exactly when doing less does not change
the answer:

> **For any history of raw-store commits, cut into runs any way, the
> render store after the last run holds what one cold render of the raw
> store at its last commit would.**

"Holds the same" means the same rows with their per-run stamps left out,
not the same file or commit hashes. Two consequences are checked on
their own, because a run can break either and still end right:

- **Every commit is truthful.** Each commit the render store makes,
  at a checkpoint or at the end, is the cold render of some of the
  render units at the pinned commit: never a document with its rows
  deleted and not yet written again, never a store emptied ahead of a
  refill. That is what the SQL transaction around each batch of
  documents is for (`etl/render/src/indexed_markdown.rs`).
- **The index converges.** `unified_grid_and_search_index` pinning any
  sequence of those commits ends equal to the render store, deletions
  included.

The framework keeps the property for a provider that keeps five
promises:

1. **Pure.** What it emits is a function of the raw store at the pin and
   of its `render_params`: not of the clock (`RenderCtx::now` is stamped,
   never read to decide anything), not of files outside the raw store,
   not of iteration order.
2. **The changed set is complete.** Every render unit whose documents
   would differ between two commits is named, by its forward scan or by
   the rows it declared. A miss is silent: the run succeeds and the
   document is simply old.
3. **Removals are declared.** It declares every render unit it looked
   at, with nothing when the unit's rows are gone (§"The sweep").
4. **Byte-stable.** The same render unit rendered twice from one commit
   gives identical rows, so a steady-state run moves nothing.
5. **`consumed` exactly when it read.** It reports the commit it pinned
   when it read the store, and nothing when it could not.

Two tests hold it:

- **`datalib_step/src/render_model_test.rs`** drives the real driver,
  store, diff scan and index with one fake provider over a synthetic raw
  store of three tables, one for each shape a render unit query meets: the
  unit itself (`parents`), rows that reach it through a foreign key
  (`children`), and a table every document reads and none owns
  (`authors`). Over twelve fixed seeds it commits random mutations, cuts
  them into random runs with random checkpoint cadence, bumps the version
  or the params now and then, fails runs partway, and after every run
  checks the store against a reference renderer's cold answer, every new
  commit for truthfulness, and, after some runs and at the end, the index.
  A failure prints its seed and the whole history.
- **`tests/fixtures/render_contract_test.py`** checks promises 2 and 3 for
  every real provider against its TNG store, by mutating each raw table
  (its docstring lists the mutations), rendering incrementally and cold,
  and comparing. Breaches it knows of are named in `KNOWN_GAPS`, each
  with why, and the list is exact: an entry that stops failing fails the
  run until it is removed. A table the fixture leaves empty has nothing
  to mutate and is passed over without a word, so a provider's gap in a
  table the fixture never fills goes unseen. Promise
  1 has no general test: the mutation loop finds the common breach,
  reading the clock to decide something, as a nondeterminism, and the
  rest is review.

### Memory: a page at a time

A render's memory should follow how much it reads at once, not how big
the source is. A render that loads every row first, parses every
payload and then renders needs several GB for a large mailbox or
workspace. One that loads a single render unit per query pays the fixed
cost of a query for each one (a lookup from the tree's root, a round
trip), and that cost dominates a source with 200k threads. Between
the two is a **page**: a run of a few hundred render units read and
rendered together, then dropped before the next.

`datalib_etl_render::pages` holds the two pieces a render pages with:

- **`RenderUnitPages`** hands out render unit keys a page at a time. On a full
  walk it reads the primary table's key column in order, starting
  after the last key it handed out (keyset pagination): one ordered
  range scan per page, nothing kept between pages but that key, and
  the pool's one connection free between them. On a narrowed run it
  hands out the stale set, sorted, in pages of the same size. A key's
  rows never straddle two pages.
- **`batches`** cuts a page's blob loads by bytes, so one render unit with
  large attachments cannot pull a whole page's worth into memory.

The provider loads what hangs off a page's keys (rows, then join rows)
with one bound JSON array each (`etl/README.md` §"Binding a set of
values"), renders it, and declares the page's render units before reading
the next, so nothing about a render unit outlives its page. Only the small
tables every render unit looks up (users, channels, accounts, mailboxes)
are read whole, up front. Email renders this way, and so do Slack and
Discord (`datalib_etl_chat_common::render_paged`) and
Claude Code and Codex (`datalib_etl_agent_sessions_render::render_paged`),
which size a page by how many records it holds, counted from the index
without reading one. The other renders still read their rows whole
([#1959](https://github.com/imbue-ai/datalib/issues/1959) lists them).

Blob bytes are the larger part of most stores, and a render that still
reads its rows whole can at least leave them where they are until they
are needed. Every other render with attachments builds its documents
from the rows, narrows to the ones this run renders, and then loads a
batch of documents' attachments at a time, cut by `blob_mb`, just
before the batch renders. For a chat-shaped source that is
`datalib_etl_chat_common::blobs`: `ChatBlobs` reads a chat's refs off
its attachments, weighs them with `BlobBundle::sizes_many` (the blob
store's `byte_len`, no bytes read), and `render_batched` loads and
renders one batch at a time; `render_batched_at` reopens the raw store
at the commit a parse read (`BlobsAt`) for a render whose parse has
closed it. Beeper weighs its documents from what its parse recorded,
and Notion loads its pages' bundles through `PageBlobs`. A person's
photo is only ever a link, so no render loads one. A chat's bundle is never
split, so one very large chat is still one batch. Only email's bundles
hold bytes, its `.eml`s, which it renders from; everyone else's name the
blob store that holds them.

**The size is the person's to choose.** A config's `[env]` table sets
`DATALIB_STEP_RENDER_PAGES = "<render_units>,<blob_mb>"`: `render_units` per page
and `blob_mb` per blob batch, either side left empty for its default.
The runner hands `[env]` to every step, and a processor reads the
value as `RenderCtx::page_size`. Changing it changes no
document, so it is not a render param and re-renders nothing. The
defaults, 500 render units and 64 MB, sit where a page's queries already
cost little next to rendering it.

What a page bounds is the reading side. On a synthetic Slack store of
240k threads a cold render peaked at 0.7–1.3 GB paged, against
2.6–3.5 GB reading everything first, in the same time. What is left is
mostly the writer: doltlite holds each table's pending edits in memory
until they reach 64 MB or the SQL transaction commits, and each
connection keeps a 64 MiB page cache. Committing the SQL transaction
more often trims the peak but leaves more pages that later checkpoints
pin on disk, so the writer does not: committing every 32 MB of documents
took that store's peak from 949 MB to 835 MB and its file from 6.9 GB to
10.4 GB, and a 16 MiB page cache as well took it to 726 MB.

### Rendering the delta itself

If you can render a collection of things, consider rendering the
difference between two versions of it. A **diff group** does that with
no second renderer: the source's render processors run at two raw
commits into a collecting sink — each pass the incremental render the
sync step already does, so the cost tracks the render units that moved, not
the store — and the two sides are subtracted per document
(`datalib_etl_render::diff`): rows keyed by `uuid` become `added`,
`removed`, `modified` (naming the columns that moved) or `unchanged`;
sections keyed by their `data-section-uuid` are wrapped in
`diff-added` / `diff-removed` / `diff-modified` bands, with a
line-then-word diff inside a modified one. The result is written as an
ordinary render tree, every uuid re-minted under the diff group so it
never claims a source row's id.

What this asks of a renderer is nothing beyond the contract above: a
render must be a pure function of the raw rows at a pin (no per-run
stamp in a row), every row and section must carry a stable uuid, and
the renderer must say what its sections are (`RenderedMarkdown.sections`,
concatenated they are the document's markdown) rather than leaving the driver to
parse them back. A renderer that declares no sections has its
documents diff as one block.

How the step gets there (`datalib_step/src/render_diff.rs`):

- **One pass per side, each scanning from the other.** The `to` side
  renders, at `to`, the render units the provider's forward scan names
  over the raw diff from `from`; the `from` side does the same at
  `from`, scanning from `to`. Both are needed because a row deleted
  between the two has no render unit at `to` to load it into, so only
  the pass at `from` names it, and an added row only the pass at `to`.
  Each pass is handed an empty stale set rather than none, so the scan
  alone decides (none would mean "render everything"), and the
  `render_inputs` reverse lookup is not used.
- **Every run is a full walk of its pair**, so a changed pair sweeps out
  what the old one produced. A document the scan named that renders the
  same on both sides is not a document of the diff. A removed row is the
  `from` side's row as it was, so the grid can show what left.

What a diff therefore cannot show:

- **A change only in a lookup table** (a user renamed in `users`, no
  message touched) names no render unit in a forward scan, so the diff
  shows nothing for it. The ordinary render catches it through the
  reverse lookup, which the diff does not run.
- **A source rendered as one unit by staleness alone** — AirVisual,
  Garmin and YoLink, one page of plots each — renders nothing under a
  diff: with an empty stale set its page is never stale. A source that
  renders nothing at all is refused.
- **What the ingest never saw**: [ingestion § "The delta as a source:
  diff groups"](data_architecture_ingestion.md#the-delta-as-a-source-diff-groups).

## 6. Timestamps

If [object identity](data_architecture_ingestion.md#object-identity-ship-of-theseus-on-uuids) is "UUIDs give global object identity," this is its temporal sibling: **timestamps give global temporal ordering** across every provider that has a time-shape to its data. That ordering is what makes the union grid time-sortable and makes `before:` / `after:` mean the same thing across Slack and GitHub and Notion.

The principle: **every event-shaped `GridRow` carries an ISO-8601 timestamp with explicit offset.** What `created_at`, `modified_at` and `touched_at` each mean, for a document row and for a row inside one, is in [`grid_rows.md`](grid_rows.md#created_at-modified_at-and-touched_at). For any of them:

- **Real upstream timestamp when one exists**, on the clock it was written on: an email's `Date:` offset, a calendar event's zone.
- **Microsecond-bump for synthesized timestamps.** Blocks and sub-items that lack their own timestamp (chat blocks within a message, ChatGPT messages within a conversation that only has a create_time) get a synthesized one by bumping microseconds off the parent's stamp. This keeps within-parent order stable across re-runs and guarantees no collision with real stamps (real timestamps don't carry per-row µs precision from upstream).
- **Strict RFC 3339 with an offset, never naive.** A naive timestamp can't be globally sorted alongside a `+02:00` one without a hidden timezone assumption.
- **`Z` means the clock is unknown, `+00:00` that it was UTC** (AGENTS.md §"Timestamp convention"). A message whose source records only an instant is written `Z`, in its header and its rows' `created_at` (below).

The crate that enforces all of this —
`IsoOffsetTimestamp::now_local()`, `parse_strict`,
`parse_with_assumed_utc`, `bump_micros` — is shared with download and
documented in
[the ingestion doc](data_architecture_ingestion.md#single-source-of-truth-datalib-time).

### A stamp on a page

**Every time a document shows is a `<time>` element written by
`datalib_etl_render::message`**, so a reader sees one form whatever the
source:

```html
<time datetime="2026-01-06T08:00:00+01:00" title="2026-01-06 08:00:00 +01:00">Tue Jan 6th, 2026 at 08:00</time>
<time datetime="2369-03-18">Tue Mar 18th, 2369</time>
```

The text is the short form any markdown viewer shows; `datetime` is the
machine-readable value, which the viewer reads to draw the stamp; the
`title` is the whole instant. A message header writes it with
`timestamp_html` (class `msg-ts`); anywhere else, `stamp_html` for an
instant, or `stamp_text_html` for a stamp a record keeps as text: an
RFC 3339 instant, a `YYYY-MM-DD` day, or else the text as written. A
stamp keeps the offset the source gave it: a message header takes it
beside the instant (`MessageHeader::offset_secs`, which chat-common fills
from `NormalizedChatItem::date_offset_secs`), and the message's grid row
keeps the same offset in `created_at`. A message whose source names no
clock (epoch milliseconds, a service's own `Z`, a Slack `ts`: a profile's
`tz` is where its author is now, not where they were) is written in `Z`
(`datalib_time::with_unknown_offset`), in the header and in
`created_at`, and the viewer reads it on its reader's clock. A real UTC
clock (an email dated `+0000`) stays `+00:00`. A grid
row's text is not markdown and keeps plain words
(`datalib_time::short_ts`). Not `<time>`s yet: a contact's details
("Friends since"), and a Garmin device's last sync, which Garmin writes
without an offset. What a viewer may rely on is
[the document contract](../user/rendered_documents.md#a-time).

### No fabricated timestamps
A corollary of "[don't make up data](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)", called out because timestamps are the easiest place to violate it:

- When upstream gives us no timestamp and we can't pick one up from a parent (no `bump_micros` source), `created_at` is **null**. Not "epoch," not "now," not "midnight UTC of the row's date."
- When upstream sends a stamp that will not parse, the row's stamp is null and the failure is a `problems` row (`own_stamp_ms` in `chat-common`, `GridRowBuilder::build_or_record`), so an empty time is never mistaken for a record that had none.
- Fallback paths that synthesize a value when upstream is silent are anti-patterns even when they "look plausible." They mask incompleteness in ways the consumer can't tell apart from real data.

Perseus breaks this rule: it stamps each section 2026-01-01 plus an offset derived from its locator (`render_synth_ts` in `perseus_render`), to give the text an order.

### Entities without a time-shape
Some upstream object types genuinely don't have a meaningful timestamp:

- **Contacts (vCards).** A person doesn't have a creation event; they exist. The vCard's `REV` field is sometimes set, but most contacts lack one.
- **Immutable corpora** such as a classical text: the corpus is upstream-frozen, and per-section "timestamps" would be nonsense. Perseus, the one such source, is the exception named above: its render gives every section a made-up stamp.

For these `created_at` is **null** (Perseus aside), and `before:`/`after:` exclude the row — the principle is "**event-shaped** rows get real timestamps," not "every row everywhere." A new provider should decide explicitly which of its row types are event-shaped and document the source of `created_at` for each.

## See also

- [ingestion § "Noticing when the *upstream* loses data"](data_architecture_ingestion.md#noticing-when-the-upstream-loses-data)
  — the download side of deletion, of which §5's sweep is the
  render-side half.
- [`step_protocol.md`](step_protocol.md) — where R2's third category
  has to be written down to mean anything.
- [`grid_rows.md`](grid_rows.md), [`edges.md`](edges.md),
  [`entity_ids.md`](entity_ids.md) — the tables and the id recipe.
