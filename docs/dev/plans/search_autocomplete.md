# Search autocomplete: a person, a source, a value as a chip

*Proposal (2026-10-09). Step 1 of the order of work is built. It builds on the search terms
file from [`search_tabs.md`](search_tabs.md) §"The search terms" and changes
what that plan says the search terms hold; the facts it cites about the tree
were read that day.*

While a person types `from:`, `to:`, `source_id:` or another key, the
search bar offers the values that key can take. Picking one puts the
value in the query and draws it as a chip: the person, the source, as
the rest of the app draws them ([`../chips.md`](../chips.md)). Typing
on without picking keeps the old behaviour, a value matched as text.

## What the tree does today

- **A keyed filter is an exact match.** `author:Picard` is
  `author = ?` (`unified_index/src/db.rs::build_where`). No key matches
  part of a value.
- **There is no `from:`, `to:` or `cc:`.** The author's handle is
  `author_handle:`, exact. The grammar has AND and a `-` per term, and
  no OR.
- **The search terms hold one person per row**, its author (`SearchTermKind::From`),
  and are read by the Fields tab and for a query made entirely of
  identifiers (`applets/src/unified_index/search_terms.rs`). Recipients, mentions and
  participants are `search_tabs.md` step 4, not built.
- **Two search bars search the grid**: the toolbar's `CommandBox.vue`,
  which opens a search card, and the card's own input in
  `GridCard.ce.vue`. The run log (`RunLogPanel.ce.vue`) and the map
  (`UmapCard.ce.vue`) each have one more, over their own tables.

## The query text stays the truth; a chip is how a value is drawn

Nothing new is added to the grammar. **The value decides what a term
means**, and the field draws it from that:

| value | meaning | drawn as |
|---|---|---|
| a handle as `datalib_handle` spells it: `email:riker@enterprise.org`, `slack:T01/U02` | exactly that handle | a person chip |
| `contact:<contact_id>` | any handle linked to that contact ([Contacts](#contacts-expanded-when-the-search-runs)) | a person chip |
| a source's id, on `source_id:` | that source | a group chip |
| anything else | part of a handle, or of a name the handle has been seen under | text |

So a saved query, a URL, a pasted query and an agent's query all still
work as text, and `from:rik` is still a search, a partial one. A
person who wants a partial match simply doesn't pick a suggestion.
`contact` is not a handle kind (`HandleKind` is `email`, `tel`,
`slack`, `signal_aci`), and must never become one.

**One grammar change**: a value may hold `:` unquoted. `split_term`
already cuts a term at its first colon, so `from:email:riker@…` reads
back as key `from`, value `email:riker@…`; today `quote` (and the UI's
`quoteValue`) wrap any value with a colon in quotes. Both sides change
together, and `term_round_trips_through_parse` covers it.

## The keys

| key | aliases | matches | suggests |
|---|---|---|---|
| `from:` | `author_handle:` | the `from` terms | people |
| `to:`, `cc:`, `bcc:` | | their own kinds | people |
| `with:` | `involves:` | any person kind (below) | people |
| `source_id:` | | the column, as today | the configured sources |
| `kind:`, `change:`, `is:` | | as today | their words |

`with:` is a person in any role. A person kind is `from`, `to`, `cc`,
`bcc`, `participant`, `mention` or `reactor`; which kinds count is one
pure function beside `SearchTermKind::affinity`. Typing `@` at the start of a
word opens the same people suggestions and writes `with:` with the
value picked.

A key served from the search terms is declared beside the grid's column keys
and read through the same grammar (`search_tabs.md` §"How a search
uses it"), so an unknown key is still refused by name. `-to:x` is the
rows not in `to:x`; a key repeated is both, as every key is now.

## Contacts: expanded when the search runs

`from:contact:<id>` is answered by reading the contact's handles from
the contacts store and matching any of them. The `unified_index`
applet opens the store read-only at its head (a reader, under the one
writer rule; `../../AGENTS.md` §"Doltlite"), reads the contact's
handles, stopped ones included since they still name the person in old
messages, and follows `merged_into` to the survivor.

- **Live.** A link made a moment ago changes the next search, as it
  changes chips at once. Nothing in the index moves when a link does.
- **The results cache** (`results::Key`) adds the contacts store's head
  for a query that names a contact, or a cached page outlives the link.
- **No contacts app configured**: `contact:` is refused, saying so,
  never matched against nothing.

This replaces the `contact:` filter of
[`contact_linking.md`](contact_linking.md) §"Search", which found a
contact through a snapshot of the store rendered into the index and so
followed a link one sync later. That plan's snapshot is still how a
contact would become a row of its own, findable by name; the filter no
longer waits for it.

## Suggestions

**One mechanism for every searchable table.** Each answers the same two
routes beside its search, and the field is handed only that base:

| route | answers |
|---|---|
| `<base>/keys` | each key, its aliases, and what its values are (`datalib_columns::KeyValues`: `text`, `words`, `source`, `group`, `step`, `stamp`; `person` comes with step 2) |
| `<base>/values?key=&typed=&q=` | the key's values holding `typed` (case-blind), among the rows the rest of the query `q` keeps, most rows first, with their counts; a closed set's words, in their own order |

The bases are `/applet/unified_index/search` and
`/applet/unified_index/problems` (`applets/src/unified_index/columns.rs`:
`keys_of`, `value_source`, read from each table's `SearchTable`) and
`/api/log` (`datalib_runs::log_values`, read from the log's own
`KEYS`). A column's values are one `GROUP BY` over the rows the rest
of the query keeps (`unified_index/src/group.rs::values_sql`), so
`source_id:slack channel:` offers Slack's channels. Free text does not
narrow them, which would be a qmd search per keystroke.

A person key's values (step 2) come through the same route: your
contacts whose name holds `typed` first (the contacts app's
`GET /search`, each with its handles), then handles that appear in
that role (any person kind for `with:`) whose value or a name they
were seen under holds `typed`, ranked by how many rows name the handle
in that role. The chip resolves as every person chip does (`people` in
`contacts.ts`).

**Two things the search terms file gains for this:**

- **The names each handle was seen under**, with counts: a small table
  `grid_index` fills from `source_contacts` and `source_contact_handles`
  and from `(author_handle, author)` on each row. It is what lets
  "Will" find `email:riker@…`.
- **A substring match.** The FTS5 index keeps a handle as one token
  (its tokenizer treats `@ . - _ + : /` as letters), so it matches
  `email:rik*` but never `rik` inside one. Either a second FTS5 index
  with the `trigram` tokenizer over person values and names, or a
  `LIKE` scan of the names table. Measure both on a real root against
  a 50 ms budget per keystroke.

## The field

One component, `SearchField` (`ui/src/search/`), used by `CommandBox`,
`GridCard` and `RunLogPanel`; the map's box can follow.
It is built on **CodeMirror 6** (MIT; the UI bundle's notices come
from `scripts/third_party_notices.sh`), a single-line editor whose
document *is* the query text:

- **A chip is a decoration** over the value of an identity term
  (`Decoration.replace` with a widget), drawn where it was typed; the
  key stays text before it (`from:` beside `to:` says the role). The
  cursor steps over it as one unit (`atomicRanges`). Copy is the
  text, undo is the editor's. The widget's DOM is `entityCell`, the
  grid's own chip (and `chipCell` for a person), drawn with the host's
  `chip.css`.
- **The word being typed stays text** until the cursor leaves it, so
  `source_id:sla` is not drawn as a source named "sla"; a pick from
  the menu is drawn at once.
- **Suggestions** are CodeMirror's autocomplete, fed by `/keys` and
  `/values`, a source, group or step drawn as its chip there too.
  Taking a key opens its values. The menu opens with nothing chosen,
  so Enter still searches what was typed. ↓ and ↑ choose; Enter or
  Tab takes the chosen one; Tab with none chosen takes the first; Esc
  closes the menu.
- **Backspace** at a chip's end deletes it whole.
- **A click on a chip** selects it whole, so Backspace deletes it and
  typing replaces it. **A double-click** opens it to be edited: its
  value as text, selected, with the key's values offered. Elsewhere a
  double-click opens what a chip names; in a text field editing is
  what a person expects, and the menu still opens it.
- **A chip's right-click menu** (`search/chipMenu.ts`) is the field's
  entries, Edit as text and Exclude (or Include, taking the `-` off),
  then the chip's own (`entityMenu`: copy, open its dashboard or log,
  browse). People's chips will add changing the key (from, to, cc,
  with).
- **For tests**, the editable element carries `data-testid`,
  `role="searchbox"` and `data-query`, the query as it stands. A spec
  types into it with `typeInto` (`tests/e2e/grid-helpers.ts`), and
  reads it with `toHaveAttribute("data-query", …)`: Playwright's `fill`
  leaves it unchanged in WebKit, and `toHaveValue` has no value to read.

Hand-written `contenteditable` is what this avoids: caret, IME and
undo handling that WebKit, which the desktop app runs, gets wrong in
its own ways, and where most of our e2e flakes have come from.

## The wide columns and the tall search terms

`grid_rows` has twelve indexes, one per key the search bar filters
on, each `(column, touched_at_utc, is_document, uuid)` so the first
page of a filter is one index walk however many rows match
(`every_filter_key_is_served_by_an_index`). With the search terms answering
person keys, some of those could go. What decides it:

- **Combining terms is not the problem.** AND and NOT on the tall table
  are intersect and except over row sets; OR, if it ever comes, is a
  union. No pivot is needed.
- **Order and paging are.** A term is keyed `(val_id, kind, row_id)`,
  so a value with 50,000 rows is all fetched and sorted for one page.
  A key served from the search terms wants its time in the key:
  `(val_id, kind, touched_at_utc, row_id)`.
- **The search terms are read as they are now**, the grid at one commit
  (`search_tabs.md` §"The search terms", "No shared snapshot"). One lookup tolerates that;
  page 2 agreeing with page 1 needs a rule.
- **Grouping** (`unified_index/src/group.rs`) and the source counts are
  `GROUP BY` on the columns, which their indexes serve.

Keep the indexes on the order and the coarse columns: touched,
`source_id`, `kind`, `is_document`, `diff_status`. Candidates to serve
from the search terms instead: `author_handle`, `author`, `channel`,
`account`, `project`, `notion_page_uuid`, perhaps `conversation_uuid`.
That splits today's `name` kind into `author`, `channel` and `account`;
the search terms file is rebuilt on any change of shape, so a new kind costs a
rebuild and nothing else. Measure each index's size and its write cost
per sync on a copy of a real root first, against the same filters
answered from the search terms.

## Order of work

1. **`SearchField`, with sources and words.** Built: the component in
   the toolbar, the search card and the run log; `/keys` and `/values`
   on all three tables; sources, groups and steps as chips; every
   column key's values from the data; the `:` grammar change. "Meaning
   only" is gone: the Meaning tab does its job. A click selects a
   chip, a double-click edits it, and its menu edits, excludes, copies
   and opens it.
2. **`from:` from the search terms**, `author_handle:` its alias, partial
   values, the names table and `/suggest`.
3. **Person kinds from renders** (`search_tabs.md` step 4), then `to:`,
   `cc:`, `bcc:`, `with:` and `@`.
4. **`contact:` values**, expanded when the search runs.
5. **The index audit**, measured, then the identity indexes dropped.

## Open questions

- A group contact (an address two people share): does
  `with:contact:<group>` reach its members' handles too, or only its
  own?
- Does a reaction count as being involved, for `with:`?
- Does partial `from:will` match the author's shown name as well as
  the names the handle was seen under? They are mostly the same names.
