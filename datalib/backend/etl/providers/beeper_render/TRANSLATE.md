# Beeper provider — Translate

The Beeper raw store is **multiplexed**: one Matrix sync gives us N
upstream networks (iMessage, WhatsApp, Signal, …) in a single doltlite
file. The Translate stage dispatches per-room on the room's inferred
`bridge_network` so each upstream service can render with its own
quirks (iMessage tapbacks, WhatsApp reply quoting, Signal disappearing
messages, …).

## Dispatch

`translate::translate_room(room, events)` (Milestone C+) matches on
`room.bridge_network` and delegates to a per-bridge module. Unknown
networks fall back to `matrix_generic`, which translates from raw
Matrix event shapes without any bridge-specific knowledge.

## UUIDs

Every id is `datalib_etl_beeper::ids` over the Matrix room, user or
event id under the store it came from, and is also the raw row's key,
so a `matrix_generic`-translated row keeps the same `uuid` if it's
later replaced by a bridge-specific translator. An event's id carries
its `timestamp_ms` in its leading bits — `docs/dev/entity_ids.md`.

## Status

- **Milestone A**: raw store only. Translate parses the raw store
  into an in-memory shape and emits zero rendered docs.
- **Milestone C**: iMessage translator.
- **Milestone D**: Signal translator.
- Later milestones: WhatsApp, Telegram, Discord, LinkedIn, …
