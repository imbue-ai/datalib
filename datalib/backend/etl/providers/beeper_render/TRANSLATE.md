# Beeper render

The raw store is Beeper Texts' own cache, mirrored table for table
([`../beeper/INGEST.md`](../beeper/INGEST.md)), so everything that reads
meaning into it is here (`src/render/parse.rs`):

- **A room** is a `threads` row. Its network comes from the account id
  (`local-signal_…` is Signal, `slackgo.…` is Slack, `googlechat` is
  Google Chat); a thread on no known network, such as a Beeper `$space`,
  is no room. The render step's `networks` param keeps only the rooms of
  the networks it names, and every network when unset.
- **A message** is an `mx_room_messages` row. Its type is Beeper's own
  (`TEXT`, `IMAGE`, `FILE`, `HIDDEN`, …); its text is `text_content`, or
  the `text` in its `message` JSON.
- **A reaction** is either an `mx_room_messages` row of type `REACTION`
  or an `mx_reactions` row.
- **A person** is their `participants` row in that room: named by
  `full_name`, then `nickname`, else their Matrix id. Their handle is
  `slack:<team>/<user>` for a Slack user, whose Matrix id the bridge
  spells `@slackgo_<team>-<user>` in lower case (the account's owner in
  a Slack room, who appears under their Beeper id, by the Slack user the
  room was bridged for), else the first number or address
  `participant_identifiers` lists for them; a Matrix id alone gets none.
  Every person with a handle is a source contact of the chat, with those
  numbers and addresses and their avatar (`img_url`) where the app
  cached it; render sniffs its image type, since the cache's file names
  have no extension. A chat with no `title` is called by its people
  other than the owner.
- **An attachment** is an entry of the message JSON's `attachments`. Its
  `mxc://<server>/<id>` names the cached file `media/<server>/<id>`, and
  an on-device bridge's `localmxc://<bridge>/<id>` names
  `media/localhost<bridge>/<id>`; the file's bytes are in the blob CAS
  when the download copied it, and the attachment draws as a placeholder
  when it did not. An attachment with no id, or an id of another scheme,
  is a problem on its document.
- **A link back to the network**: a Slack room links to its channel
  through Slack's `app_redirect`, from the bridge's `<team>-<channel>`
  id. Other networks' rooms, and every message, have none: the cache
  holds no id that makes one.

Render groups the rooms by network and calls chat-common once per
network (`src/render/normalize.rs`), because the `grid_rows` taxonomy is
per-network too: the `source_label` is `Beeper:<Network>` (so
`LIKE 'Beeper:%'` selects everything Beeper delivered and
`LIKE '%:Signal'` every Signal chat whichever source brought it), and
the kinds are `<Network> Chat`, `<Network> Message` and
`<Network> Reaction`.

The markdown layout, `path_prefix`, orphan reactions and `LAYOUT_VERSION`
are chat-common's ([`chat-common/README.md`](../../chat-common/README.md)).

## Documents are room × period

A document is one room's events in one period: `month` by default, or
`day`, `year` or `all`, set by the render step's `period` param. A
reaction is filed under its target's period however late it arrived;
one whose target is not in the store at all is an orphan reaction. A
reply is drawn as a quoted "in reply to" line naming the upstream id it
points at. A `HIDDEN` event gets a one-line system note, so a room made
only of those still renders.

Each document declares the `threads`, `participants`,
`mx_room_messages` and `mx_reactions` rows it read, so a room whose
messages all left the store loses its documents; a change to a
person's `participant_identifiers` reaches every room they are in.

## Ids

Rooms and events are keyed by their Matrix ids. Every rendered id is
`src/render/ids.rs` over that Matrix id under the configured source; an
event's carries its timestamp in its leading bits
(`docs/dev/entity_ids.md`).
