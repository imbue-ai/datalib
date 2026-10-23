# Dispatch on kind: one registry, and applets can join it

*Mirror of the "Dispatch on kind" tab of the [datalib links](https://claude.ai/code/artifact/dd31c9e3-8ef9-4185-a85f-0c29a1318117) Claude Doc, which is the source of truth while this plan is under review. Edit the doc and re-mirror this file; don't edit it here. Proposal (2026-10-22); nothing here is built.*

Adding a new kind of thing a link can name, such as a document or a sync run, means touching about eight places across the Rust backend and the UI. And the UI finds out whether the contacts app is configured by reading the gateway's error message for its name. This tab proposes one registry of kinds. Each kind has a few **facets** (how it is named, what a chip shows for it, what opening it does, where its bytes come from), and each facet is answered either by datalib itself or by an applet that says it can. It came out of comparing notes between the links work (first two tabs) and the chips audit (PR #1304).

## 1. What a kind needs today

Every kind answers the same questions, each in a different place, mostly as a branch per kind:

| Facet | Where it lives | How it is chosen |
| --- | --- | --- |
| **Name**: how a link to it is written and read back | `chipLinks.js`; `Handle::to_uri`, `datalib_columns::Entity`, `contact_uri` in Rust | a branch per kind, on each side |
| **Mark**: which links in a page become chips | the markdown plugin in `chipLinks.js`; the sanitizer's list of schemes | a branch per kind |
| **Resolve**: what the chip shows (name, icon, status) | one `Resolver` per kind in the UI: `people`, `contactsById`, `entities` | endpoints written into the UI. The contacts app is found by reading the gateway's 502 for the words `no applet "datalib_contacts"` |
| **Look and menu** | pure functions per kind in `contacts.ts`, `entities.ts` | a branch per kind |
| **Open**: what a double-click opens | `chipCardSource` in `chipActions.ts`: person → person card, group → sync dashboard, step → log, contact → nothing yet | a switch. Separately, a Manage row's Browse picks an applet's card by its id in an if-chain (`appletBrowseSource`) |
| **Serve bytes** | `datalib-http`'s `/data/…/<kind>/…` (`data.rs`) | an enum with one kind, `blob` |
| **Stays fresh**: which live updates move it | `movesPeople`, `movesEntities` | a function per kind |
| **Deep link**: opening a document from a pasted link | `router/deeplink.ts`, `chatLink.ts`, a document card's edge links | a separate path that only documents use |

The kinds, and who answers for each today:

| Kind | Resolve | Double-click opens | Bytes |
| --- | --- | --- | --- |
| a person (by handle) | the search applet's `POST /people` (each source's record), plus the contacts app's `POST /resolve` (the contact a person linked) | the person card | — |
| a contact | the contacts app | nothing yet, for a contact with no handle; a contact with one is drawn as its best handle's chip, which opens the person card | — |
| a group | `datalib-http`'s `POST /api/entities` | the sync dashboard | — |
| a step | `datalib-http`'s `POST /api/entities` | the step's log | — |
| a document (proposed `doc`) | — | the document card, by a separate path | — |
| an attachment (`blob`) | — | — | `datalib-http`'s `/data/` route |

Contacts add one more kind of branching, but at render time: each source turns its own records into `NormalizedContact`s, and the read side is the same for all of them, keyed by handle. That part already works the way this proposal wants the rest to.

## 2. The proposal

1. **One registry, one row per kind.** A row says how the kind's names are written, which resolver answers for it, which card it opens, its menu, which route serves its bytes, and which live updates move it. It lives beside the names it describes: the URI crate in Rust and its mirror in the UI (first tab, §5). The branches in §1 (`chipCardSource`, `appletBrowseSource`, the sanitizer's schemes, the resolver instances, the bytes route's enum) become lookups in it.
2. **A facet is answered by datalib or by an applet.** An applet already writes metadata about the cards it offers to `system/frontend/<id>/`. That metadata gains a short **manifest** of what the applet handles, for example `resolves: ["contact"]` or `opens: [{kind: "group", type: "garmin", label: "Garmin days"}]`. `datalib-http` merges the built-in rows with every running applet's manifest and serves the result at `GET /api/kinds`. The UI builds its resolvers and menus from that answer, so the contacts app is found by its manifest instead of by an error message.
3. **What an applet may contribute stays narrow.** A card to open, which is data, never code (AGENTS.md §"Card source is data"). Answers to a resolve request, over the gateway. Bytes, over the gateway, with the same rules the `/data/` route applies (second tab, "Serving them"). Opening a card still never writes.
4. **Some facets take several contributors, some take one.** A person's chip already merges two answers, each source's record and the contact a person linked, so resolve answers merge by name. Every applet that can open a kind is offered in the menu ("Open in Garmin days"), and the built-in card stays the double-click unless the person picks another. Bytes have exactly one owner per kind.
5. **Deep links come for free.** Opening a `datalib:` name, whether pasted into the app or handed over by the operating system, goes to the registry's open facet. `datalib:doc/…` and `datalib:group/…` then open the way a double-click on their chip does, and the separate document path in §1 goes away.
6. **Shared federation.** Names and addresses are separate registries, but they are filled the same way, in five steps:
   - **Contribute.** An applet has one manifest, with a names section (`resolves`, `opens`) and an addresses section (`serves`). A step contributes without a manifest, by writing data in an agreed shape: a step that writes a `blobs.sqlite` in the contract's shape has its bytes served already. Applets contribute as running processes, steps as data at rest.
   - **Merge.** One merge, in `datalib-http`, where each facet declares its policy: `one` (serves), `many-merged` (resolves) or `many-offered` (opens).
   - **Conflict.** A second claim on a `one` facet is a diagnostic that costs only that claim, like a config entry the loader cannot use. Where several claims are allowed, they are taken in config order: the person controls it, and it is the same every time.
   - **Liveness.** One live update (`kinds_changed`) when an applet starts or stops or the config changes; the UI asks again on it.
   - **Absence.** A facet with no live contributor answers "absent", never an error message to be parsed.

   What stays different is who reads each registry, and what a contributor may hand back. The UI reads names, for chips, menus and deep links; the server reads addresses, to route bytes. A names contributor returns data or a card to open; an addresses contributor returns bytes served from our own origin, which need the sandbox and content-type rules.

Two registries stay distinct, as in the first tab: a **name** (`datalib:<kind>/…`) is resolved, drawn as a chip, opened and given a menu; an **address** (`/data/…/<kind>/…`) hands out bytes. A chip's menu can offer "Open file" by asking the bytes route, but the two are looked up separately.

## 3. Before, after, and what a person notices

| Change | Before | After | What a person notices |
| --- | --- | --- | --- |
| Adding a kind of chip | about eight places, in Rust and the UI | one registry row and its handlers | Nothing. |
| Finding the contacts app | the UI reads the gateway's 502 for the applet's name | the applet's manifest says it resolves contacts | Nothing; rewording an error can no longer break contacts. |
| An applet offering a card for a kind | only from a Manage row's Browse, by its id in `appletBrowseSource` | its manifest lists what it opens; the menu offers "Open in …" | New menu entries where such an applet is configured. |
| Opening a pasted link | documents only, by their own path | any `datalib:` name, through the same open facet as a double-click | A pasted group or step link opens too. |
| A script or an agent | — | can read `GET /api/kinds` to learn what this root can resolve, open and serve | Nothing, unless it asks. |

## 4. What to build when

1. **Now, with no applets in it:** the registry as a table beside the URI crate and its UI mirror, with today's branches reading from it, and the `doc` kind added, which both the links work and the chips audit want. Nothing changes for a person.
2. **With the first applet that needs it:** manifests and `GET /api/kinds`. The likely first customers are the contacts app's resolver, and the per-source "main" card the chips audit proposes for a group's double-click.
3. **Only when a kind needs more than the blob-store contract:** bytes served by an applet (second tab, §7's triggers).

## 5. Open questions

- [ ] **Does an applet declare what it handles in its own metadata, or does the config say?** Metadata keeps what an applet can do with the applet. The config, if anything, could switch a claim off.
- [ ] **Do resolvers share one answer shape** (label, icon, detail, status, the actions a menu offers and why any is blocked), or one envelope with a body per kind? The evidence points to the envelope: a person's answer is a list of each source's record of them, and only a group's or step's answer needs the menu's actions, which the chips audit wants `/api/entities` to return.
- [x] **Can a step register too, not only an applet?** Answered: a step already does, without a manifest. A step that writes a `blobs.sqlite` in the contract's shape has its bytes served. Steps contribute by an agreed file shape, applets by manifest (§2, item 6).
- [x] **When two applets claim the same open or resolve facet, what order do their offers and answers take?** Answered: config order. The person controls it, and it is the same every time.
