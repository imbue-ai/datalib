# Beeper render

The Beeper raw store is **multiplexed**: one Beeper Texts install
carries many upstream networks (Signal, Google Chat, WhatsApp, …) in one
doltlite file, each room tagged with its canonical `network`
([`../beeper/INGEST.md`](../beeper/INGEST.md)). Render groups the rooms
by that network and calls chat-common once per network
(`src/render/normalize.rs`), because the `grid_rows` taxonomy is
per-network too: the `source_label` is `Beeper:<Network>` (so
`LIKE 'Beeper:%'` selects everything Beeper delivered and
`LIKE '%:Signal'` every Signal chat whichever source brought it), and
the kinds are `<Network> Chat`, `<Network> Message` and
`<Network> Reaction`. There is no per-network translation: every
network's events are read from the same bridge-agnostic columns.

The markdown layout, `path_prefix`, orphan reactions and `LAYOUT_VERSION`
are chat-common's ([`chat-common/README.md`](../../chat-common/README.md)).

## Documents are room × period

A document is one room's events in one period: `month` by default, or
`day`, `year` or `all`, set by the render step's `period` param. The
`(room, period)` split is computed in SQL (`src/render/parse.rs`). A
reaction is filed under its target's period however late it arrived;
one whose target is not in the store at all is an orphan reaction. A
reply is drawn as a quoted "in reply to" line naming the upstream id it
points at. A `HIDDEN` event gets a one-line system note, so a room made
only of those still renders.

## Ids

The raw store keys rooms, users and events by their Matrix ids. Every
rendered id is `src/render/ids.rs` over that Matrix id under the
configured source; an event's carries its `timestamp_ms` in its leading
bits (`docs/dev/entity_ids.md`).
