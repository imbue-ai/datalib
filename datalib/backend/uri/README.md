# `datalib:` links

A `datalib:` link names something in a library: a page, a node of a
source's tree, a photo. It says *what* the thing is, never which screen
shows it, so it means the same pasted into a note, a chat or another app
as it does in Datalib. This crate, `datalib_uri`, writes and reads one in
the backend; [`ui/src/uri.ts`](../../ui/src/uri.ts) does the same in the
UI. Both are tested against the same cases,
[`ui/tests/uri-fixtures.json`](../../ui/tests/uri-fixtures.json), so a
case one side reads differently fails a test.

The design, and what is still proposed, is in
[`docs/dev/plans/datalib_uris.md`](../../../docs/dev/plans/datalib_uris.md).
The chips that draw a name in the app are in
[`docs/dev/chips.md`](../../../docs/dev/chips.md).

## The kinds

| Name | Names | Opens |
| --- | --- | --- |
| `datalib:doc/<page>` | a rendered page, by its uuid | the page's document card |
| `datalib:doc/<page>#<section>` | a message or section of a page, by its `data-section-uuid` | the page, scrolled to the section |
| `datalib:node/<uuid>` | a node of a source's tree, by its uuid in `nodes`: a channel, a label, a folder, a page | its node card: what is directly in it, led by its document when it is one |
| `datalib:photo/<uuid>` | a photo or video, by its grid row's uuid | the card of whichever applet says it serves `photo` (`docs/dev/applets.md`) |

Every id is a uuid (`docs/dev/entity_ids.md`), so nothing in a name can
climb out of a route. A `doc` name may carry the page's title as words
before the uuid, `datalib:doc/Ten-Forward-<uuid>`, for a person reading
it; a reader skips the words. A kind this build does not know is read as
nothing, never guessed at.

Five more kinds are written by the code that draws their chips, not
here: `datalib:group/<id>`, `datalib:step/<group>/<function>`,
`datalib:contact/<id>`, `datalib:contact_page/<page>` and
`datalib:handle/<kind>/<value>` (`docs/dev/chips.md`). Nothing outside
the app opens those yet.

A `datalib://…` link, with the two slashes, is not a name and nothing
reads it. A 🆔 copied before the `doc` kind existed was one
(`datalib://chat/<uuid>?msg=<message>`); pasted now, it opens nothing.

## Which library

**A link that leaves the app carries the library it is in**, as a query
before any section: `datalib:doc/<page>?library=<uuid>#<section>`.
The copies that put a name on the clipboard add it
(`ui/src/outsideLink.ts`): the 🔗 on a page or a message, the title
bar's Copy link and Copy name and link, a document or node chip's Copy
name and link, and a 🔗 inside a copied selection. A name inside the app
stays bare, since it is always in the library that drew it. Pasted into
the search box, a link still finds its page
(`docs/user/searching.md`).

The id is a uuid in `<library>/system/library-id`, one line. The
server mints it the first time it opens the library (`http/src/library_id.rs`)
and never changes it, so it survives a library being renamed or moved. A
library copied whole keeps its id; where two libraries on one computer
have the same id, the desktop app opens the first its libraries screen
lists. A link with no library opens in whichever library is open.

## The fixed address

**Each name has a web address on its library's server**: the name after
`/name/`, its query and section kept.

```
datalib:node/<uuid>?library=<id>        → http://127.0.0.1:8731/name/node/<uuid>?library=<id>
datalib:doc/<page>?library=<id>#<sec>   → http://127.0.0.1:8731/name/doc/<page>?library=<id>#<sec>
```

The UI's router reads it (`ui/src/router/names.ts`), opens the card the
name opens as a tab, and puts the address back to `/`, as a card path
does (`docs/dev/cards.md` § "The URL"). A node is looked up first
(`GET /applet/unified_grid_and_search_index/node/<uuid>`), so its card
has its name and its document. It opens nothing, and says why, for a
link from another library, a node the index does not hold, or a photo
when no applet serves photos.

The address lasts as long as the server's address does. `datalib-http`
run by hand listens on `127.0.0.1:8731` (or `DATALIB_BIND`), so a
bookmark keeps working; a browser that has no session yet is asked for
the server's token, as for any page. The desktop app's server takes a new
port each launch, so there the `datalib:` link is the address that lasts.

## Opening a link from another app

The desktop app registers the `datalib` scheme with macOS (Tauri's
deep-link plugin, `datalib/tauri/tauri.conf.json`), so clicking a
`datalib:` link anywhere opens Datalib. The shell
(`datalib/tauri/src/deep_link.rs`) reads only a `datalib:<kind>/<id>` of
plain characters and a `library` that is a uuid, finds the library, and
sends the main window to that library's `/name/` address. What the name
means is the page's to read.

- **The link's library is open**, or the link names none and a library
  is open: the window goes to the address.
- **The link names another library on this computer**: a dialog asks
  whether to open it, saying which library it closes. Only on Open does
  the app switch, since any app or web page can hand macOS a `datalib:`
  link.
- **No library here has that id**, or the link names none and none is
  open: a dialog says so, and nothing opens.
