# Audit: whack-a-mole fixes, 2026-09-30 to 2026-10-05

A record, not reference. It covers the 45 first-parent merges on `main`
from #937 to #988 (`7692761d3`..`fceb8199d`). It reads them with one
question: **did a fix close one case of a problem while the same
problem stays open elsewhere, and would a shared safeguard close all of
them at once?** Here a safeguard means a type, a framework default, a
shared helper, a lint check or a test over every source. General
quality up to #957 is in [`2026-10-02.md`](2026-10-02.md); this audit
repeats an item from it only where a recent PR fixed that item in one
place and not the others.

**How it was read.** Five read-only passes ran in parallel: failure
isolation and `problems`; escaping and script policy; handles and
contacts; stores and engines; UI, desktop and tooling. Each pass read
the PR diffs and then searched the tree for other places with the same
problem. The most serious claims were re-read in the code a second time.
Those are marked **checked**. **Read** means one reader traced the code
path. **Plausible** means the code was found by searching but the
failure was not traced or reproduced. No tests were run. Line numbers
are at `4f638879c`.

**What was done about it.** Items 1–5 and 7 were filed as #990, #991,
#992, #993, #994 and #995. Item 6 was not filed.

## The pattern behind most of these

In most of the 45 PRs, the fix is right for the case it targets. What
repeats is how the fix was found and how it was tested. Someone hit a
bug: the live golden rebake (#968), a real library (#976), a stray
phone number (#980). The PR fixed that source and added a test built
from that one example. The same problem in its sibling sources then
stays open until someone hits it there too.

The few tests in the window that cover a whole class of problem show
what works. #988's check compares the problem count in the logs with
the stored rows for every source. It is general, and it caught a real
fixture failure that #981 then fixed. Most items below propose a test
of that kind next to the structural fix.

## 1. Isolating a failed item is opt-in per provider

**Instances:** #981 (Slack), #987 (Takeout), #976 (grid_index).

Each PR wrote its own version of the same idea: one bad feed, file or
source fails only itself and becomes a `problems` row. #987 wrote a
`feed()` helper that turns an error or a panic into a
`RunProblem::phase`. It is private to `google_takeout/src/ingest/mod.rs`.
#981 wrote the same logic again for Slack by hand.

Two things make each provider write this logic itself.
`download_problems::report_run` (`etl/src/download_problems.rs:270`)
replaces every `listing:` and `phase:` row in the store, so a provider
has to collect all of its problems and call it exactly once. Eleven call
sites do that collecting by hand. And nothing gives a provider the
isolation for free.

**Still open.** Failures that reach only a `warn!` or `error!` and a
counter, with no row:

- **facebook, which also loses data (checked).** A file that fails to
  parse is logged and skipped (`facebook/src/ingest/mod.rs:171`). Then
  `upsert_and_prune` (`:204-234`) deletes every row of the table that
  this run did not see. Several export files fill one table:
  `album/0.json` and `album/1.json` both fill `ALBUMS_TABLE`
  (`schema_raw.rs:77-83`), and the numbered `your_posts_N.json` files
  all fill `POSTS_TABLE`. So when one of them fails, its rows that are
  already stored get deleted.
- `forge-ingest-common/src/lib.rs:207,279,327,331`: one shared engine,
  so GitHub and GitLab both lose a failed PR or scope with no row (read).
- linkedin `mod.rs:170,190,222`, `photos.rs:259,276`; sms_backup_restore
  `mod.rs:102,122,136,141`; agent_sessions `lib.rs:133` (it never reads
  `scan.errors`); yolink `mod.rs:255,271,482`; claude `mod.rs:629,737`;
  calendar `ics_dir.rs:82,127`; contacts `vcf_dir.rs:113`; email
  `mbox.rs:348,370`; notion `mod.rs:170-182`, where a failed attachment
  becomes an edge with `blake3=None` (all read).
- airvisual, beeper, signal, media, pdf, whatsapp (plausible). The last
  four log walk errors rather than calling `scan.walk_problems()`.
- **Render side:** 4 of about 30 render crates call `report_unparsed` or
  `report_document_failed`. The others drop a bad record silently or
  with a `warn!` only. Examples: whatsapp_render `parse.rs:125,175,193`,
  garmin_render `parse.rs:143,164,193` via `unwrap_or(Value::Null)`
  (plausible).
- Attachment problems from `blob_cas::flush_cas_edges` are not tied to
  the record that owns the attachment, so they never reach a document's
  banner for email, facebook, garmin or sms. `blob_cas` already knows
  the owner (`owning_id_of`); #981 did this mapping by hand for Slack
  (read).

**General fix:**

1. Move #987's `feed()` into the framework as a problem collector on
   `DownloadRun` or `FetchOptions`: `.phase(name, fut)` catches an error
   or a panic and records it as a row; `.listing(name, err)` records a
   listing that failed. The framework writes them all once at `finish`,
   and writes nothing after a stop. `report_run` stops being something
   a provider calls.
2. Add `Scan::read_each(|f| …)` for the file-backed providers: a file
   that fails becomes a row and counts as not seen when deciding what
   to prune. That fixes the facebook deletion and most of item 2.
3. Add a `lint_repo.py` check for a `warn!` or `error!` in an `Err` arm
   next to a `+= 1` under `providers/*/src/ingest`.

## 2. Each provider decides for itself whether a listing was complete

**Instances:** #937 (email's `Option<seen>`), #981 (`walks_cut_short`),
sms (`read_all && walk_errors == 0 && …`). This is 2026-10-02 finding 4,
now fixed separately in each provider. `etl/src/prune.rs:11` itself says
the decision "stays in the provider".

**Still open:**

- **Gmail labels (checked).** `plan_mailboxes`
  (`email/src/ingest/gmail_api/ingest.rs:68-88`) refiles every held
  `gmail:` mailbox that is missing from `labels.list`, and has no guard
  for an empty list. JMAP's `sync_mailboxes` has that guard. Both were
  in the same PR (#937).
- Takeout single-file feeds still return `Ok(Some(rows))` when a file
  that is not empty parses to zero rows (`youtube_watch_history.rs:87`,
  `youtube_subscriptions.rs:63`). After #987, a feed whose entries are
  all skipped still prunes its whole table (read).
- CalDAV `caldav/mod.rs:293,316,335`, and facebook as in item 1.
- `prune::record` (`prune.rs:167`) only logs a `warn!` when a large share
  of a table is deleted.
- The JMAP test (`jmap_full_resync_prunes.rs`) covers only the case
  where pruning should happen. Nothing checks that a filtered or stopped
  walk prunes nothing, which is the case that matters for safety.

**General fix:** make completeness a type. For example,
`Listing::Complete(set)` vs `Listing::Partial`, where only a walk that
finished can build `Complete`, and every prune function takes only
`Complete`. `file_checkpoint::ingest_snapshot` should refuse to prune
when a file that is not empty yields zero rows. Each walk gets one test:
a walk cut short deletes nothing.

## 3. Escaping is done by convention at each call site

**Instances:** #966 escaped upstream text in every renderer; #968
repaired grid previews that #966's new unescape broke.

After #966 the helpers are plain `&str -> String` functions in
`etl/render/src/html.rs`. Nothing stops a caller from using the wrong
one or none. There are three HTML escapers (html.rs, `etl/src/title.rs:99`,
`timeseries_render/src/plot.rs:183`). About 23 per-renderer tests all
feed the same HTML-only string, `<script>x</script>`. None feeds
markdown syntax.

**Still open:**

- **Message headers use the wrong escaper (checked).**
  `MessageHeader::render` (`etl/render/src/message.rs:46-53`) puts the
  author inside a `## ` line through `escape_text`, which escapes HTML
  only. markdown-it still parses the text between the `<span>` tags as
  markdown. So an author named `[x](https://evil.test)` or
  `![](https://…)` produces a real link or image in the header. For
  email this is the From display name, which any sender controls. Slack
  user and channel names hit the same problem (`slack_render/src/render/mrkdwn.rs:82,92`).
  `to_commonmark` (`:101-155`) also leaves Slack text's markdown syntax
  unescaped (read).
- **Front matter (checked).** `strip_frontmatter`
  (`applets/src/unified_index/mod.rs:1085-1094`) ends the front matter at
  the first `\n---` anywhere, including inside a value. Seven different
  YAML quoters write those values, and several leave newlines raw:
  chat-common `render.rs:377-395` (and account, project and external_id
  go in unquoted), pdf `yaml_str`, timeseries and contact-common
  `yaml_safe`, notion's raw source URL. A title containing `\n---`
  would end the front matter early, and the rest would be rendered as
  the body with no escaping. Whether real data can reach this, for
  example an RFC 2047 subject that decodes to a newline, is plausible,
  not shown.
- `title.rs:81,84` and chat-common `render.rs:627` escape HTML inside a
  raw HTML block but keep newlines. A blank line ends the HTML block,
  and markdown parsing starts again after it (read, checked with
  markdown-it by the pass).
- `contacts.ts:207-221` trusts a `data-handle` attribute based only on
  where it sits in the document. A forge description written raw at the
  top level, or a `</div>` inside a markdown chat item, could fake that
  position (plausible).

**General fix:**

- One small wrapper type for each place text can land, for example
  `HtmlText`, `MdInline` and `MdBlock`. Only the html.rs helpers can
  build them, and `MessageHeader`, `Title` and the chat item's text take
  the type rather than `&str`. Then a wrong or missing escape does not
  compile. Markup the source wrote itself gets an explicit
  `authored(..)` constructor.
- One `yaml_scalar` (JSON-quoted), and a `strip_frontmatter` that only
  accepts a `---` on a line by itself.
- Two tests over every renderer at once:
  1. Put a hostile string containing both HTML and markdown in every
     text field of the TNG fixtures. Run every rendered `.md` through
     markdown-it and DOMPurify, and assert the string never becomes an
     element or a link.
  2. A property test that `plain_text(escape_md_*(s)) == s`. That would
     have caught #968's grid-preview bug before it shipped.

## 4. Readers patch themselves per table for older store shapes

**Instance:** #976 adds `has_table(pool, "source_contacts")`
(`etl/render/src/indexed_markdown.rs:1191`). Two copies of `has_table`
now exist, `indexed_markdown.rs` and `whatsapp_render/.../parse.rs:558`.
There are also three other functions that decide whether a table or
column is missing: `datalib_pin::is_missing_table`,
`doltlite_raw::is_missing_schema`, which matches on the error text, and
grid_index's `written_in_another_shape` (checked). They disagree:
grid_index reports a missing column as a warning but a missing table as
an error that fails the step. `problems` already has its own copies
(`indexed_markdown.rs:1139`, `doltlite_raw.rs:1713`,
`datalib_step/src/render.rs:767`). The next table added to the render
schema will need another one.

**Why it happened (read).** #976 says the render step "doesn't re-run
when the build changes". In fact #798 does mark the render step stale
when the store's shape changes. But a step runs only if some sync
request covers it (dag README rule 3). So `grid_index` ran because
another source asked for it, and read apple-messages' store while it
was still in the old shape.

**General fix:** when a fan-in step such as `grid_index` is due, any
input that is stale only because its shape changed becomes due too, or
the fan-in waits for it. As a backstop, readers compare
`_datalib_meta.schema_hash` once and send any mismatch down the existing
`Unreadable` warning path, instead of checking table by table. Add one
test over the whole class: for each table in the render DDL, drop it and
run `grid_index`.

## 5. Handles: the rules live in one place, but changes to them do not reach stored data

`Handle` is done right (#958, #962): every `tel:`, `email:` and `slack:`
value outside tests goes through its constructors, and TypeScript never
normalizes a handle. The patching is elsewhere.

- **A rules change is spread to stored data by hand (checked).** #980
  bumped `RENDER_VERSION` in six crates by hand. Without those bumps,
  stored `contact_json` would stop deserializing. chat-common already
  solved this for its layout: `LAYOUT_VERSION` is passed in as a render
  param (`chat-common/src/render.rs:12-31`). **Fix:** a
  `datalib_handle::RULES_VERSION`, passed into the render params the
  same way.
- **Links a person made are dropped silently (checked).**
  `contacts/src/lib.rs:221-225` uses `filter_map(Handle::parse(..)?)`,
  so a stored link that no longer parses disappears with no log and no
  row. AGENTS.md asks for a compatibility path where a person typed the
  input. **Fix:** a rung on the contacts store's migration ladder that
  rebuilds each handle through its constructor and reports the ones that
  no longer map.
- **Phone rules are patched one case at a time (checked).**
  `Handle::tel` (`handle/src/lib.rs:74-93`) turns `+44 (0)20 7946 0958`
  into `tel:+4402079460958`, a different handle from `+442079460958`.
  A vCard 4 `TEL;VALUE=uri:tel:+1-…` keeps its `tel:` prefix and so
  produces no handle at all. Each provider cleans the number its own way
  before calling `tel`: WhatsApp adds the `+`, Voice strips `tel:`.
  **Fix:** one input cleaner inside `tel`, plus a property test that
  every spelling of a number produces the same handle, plus a fixture
  check that one `tel:` handle joins two sources (today only email is
  checked, `ingested_tng_test.py:987-1000`). Check the `phonenumber`
  crate's license against `deny.toml` before relying on it.
- **Two parsers for one line format (read).** #938 added RFC 6350
  unescaping for `CATEGORIES` only. `NOTE`, `ORG`, `FN` and `ADR` are
  still read raw (`contacts_render/.../parse.rs:189,213-221`). Meanwhile
  `calendar/src/ical.rs:112-205` has a quote-aware, unfolding,
  unescaping parser for the same content-line grammar. **Fix:** share
  one content-line module, as #760 did for the WebDAV client.
- Which providers write an author handle is also decided one provider at
  a time. beeper still writes `None` (`beeper_render/.../normalize.rs:189,210`),
  and Slack takes a profile's email but not its phone. The guard is a
  hand-kept list in `ingested_tng_test.py:1002-1018`.

## 6. UI: state and DOM rules that hold for one card

- **Desktop settings reset on every launch (checked).** #972 keeps zoom
  in the shell because the page's settings did not survive. The same is
  true for density, edit mode, Sources' expanded groups, the gallery's
  show-all and the hand-off opt-out, which all live in `localStorage`
  (`density.ts`, `editMode.ts`, `sourcesCardModel.ts`, `galleryView.ts`,
  `handoff.ts`). The shell binds the server to `127.0.0.1:0`
  (`tauri/src/main.rs:887`), so each launch is a new browser origin with
  empty storage. **Fix:** one prefs module backed by `/api/ui/state` or
  by the shell.
- **Moving a card's DOM breaks SlickGrid's column rules (read).** #973
  stopped tab switches from moving cards. The edit-mode operations
  (`move`, `wrap`, `unwrap`, `setLayout` in `containerTree.ts`) still
  move a card's DOM through `<Teleport>`; the PR says so. There are
  three SlickGrid hosts, and `RunLogPanel` copies `followFrame` rather
  than using `gridFrame.ts`. **Fix:** one hook in `gridFrame.ts`, run
  whenever a grid's element is re-attached, that rebuilds the column
  rules. Add an e2e that moves a card in edit mode.
- **Only cards that ask stop updating when hidden (read).** Nine callers
  pass `{onScreen}` to `subscribeLive`. `useDashboard.ts:168`, which
  runs five times, and `SearchCard` do not. Since #973 keeps hidden tabs
  mounted, a hidden Dashboard refetches its manage rows five times on
  every frame. **Fix:** a `subscribeLive` on `CardCtx` that ties
  `onScreen` to the card's own element.
- **Density reaches one grid (read).** `SourcesCard` computes its row
  height its own way (`28+8·d`), which disagrees with `theme.css`
  (`24+12·d`). The other grids have fixed heights. **Fix:** one
  `rowHeightFor(density)` used as `TableGrid`'s default.
- Status cells are drawn two ways (`SourcesCard`'s `statusCell` and
  `renderStatus`), and 2026-10-02 finding 19 (the Dashboard copies the
  row actions) is still open. #935 and #945 were the general fixes and
  did not make 19 worse.

## 7. Tooling copies

- **Write-to-temp-then-rename is hand-rolled about nine times (read).**
  Five use a fixed temp name, so two writers at once share one temp
  file: `http/src/ui_state.rs:79` (an HTTP handler),
  `manage/summary.rs:58`, `embedding_map.rs:89`, `applets/src/slack/mod.rs:53`,
  `fsindex/.../options.rs:126`. Only two fsync. **Fix:** one
  `write_atomic` in `datalib_runtime` using
  `tempfile::NamedTempFile::new_in(dir)`, and a lint check against a bare
  `fs::rename(&tmp`.
- **Launchers (read).** Whether to open a browser is decided in three
  places (`http/src/main.rs:144`, `dev.sh:166`, `serve_dev.sh:99`), and
  the default library path in three (`dev.sh:111`, `serve_dev.sh:80`,
  `tauri/src/launcher.rs`). #967 also points a dev run with no arguments
  at the person's real Default library, which 2026-10-02 finding 8 said
  to avoid. **Fix:** one sourced `dev_lib.sh`, or let the binary decide.
- #953 sets `LANG` for tests only (`.bazelrc:84`). The cause is that the
  action PATH puts Homebrew's bash first, and that affects build actions
  too. Add `build:macos --action_env=LANG=… --host_action_env=LANG=…`
  (plausible).
- `release.yml:512-522` hardcodes a rustc version that the version-pin
  check does not see. Copying out of `bazel-bin` is done four different
  ways (`hoist_node_modules.py`, `third_party_notices.sh`,
  `stage_runtime.sh`, `stage_tarball.sh`).

## Already general

These PRs fixed the whole class, not one case. Each one is a model for
the fixes above:

- #961 and #964: the script policy is decided at the gateway for every
  applet, and every document body goes through one `renderDocument` and
  `docFrame`.
- #947: one ordered key-choice rule in the mirror engine. It deleted
  Apple Messages' hand-pinned keys rather than adding a heuristic.
- #939: one naming rule (`_total`) decides what is a counter.
- #988: problems reach the log from the shared writers, and one check
  compares the counts for every source.
- #895: one reader path (`open_reader`) replaces 95 references to the
  `pinned_` views. Its cost is item 4.
- #958, #962: one `Handle` type and one `DatalibContact` type;
  `baseline_contacts` gives every chat provider contacts for free.
- #956, #935, #945, #946: one layout, an extracted `rowActions`, row
  actions the server declares, pure chart code with tests.
- #937's volatile mailbox counts are justified. They are counts the
  email rows already imply, not an unordered set.

## If only five things get done

Ranked by how much each closes per unit of work:

1. **A framework problem collector plus `Scan::read_each`** (item 1). It
   closes a list of providers at once, including facebook's deletion.
2. **`Listing::Complete` as a type** (item 2). It makes the finding-4
   bug impossible to write, not just fixed in some providers.
3. **Typed markup plus the hostile-fixture test over every renderer**
   (item 3). The test alone would have caught the header and front
   matter holes.
4. **Prefs backed by the server, not `localStorage`** (item 6). It is
   the most visible bug in this list for someone using the desktop app.
5. **A fan-in pulls in inputs that are stale only by shape** (item 4),
   which removes the reason readers patch themselves per table.
