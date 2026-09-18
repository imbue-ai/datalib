# carddav_tng_v2 — the address books one sync later

The same two `.vcf` files as `../carddav_tng/`, after three changes to
`Bridge.vcf`: Picard's card is **edited** (a home phone added, the ship
renamed to NCC-1701-E, `REV` bumped), Data's card is **removed**, and
Worf's card is **added**. `Engineering.vcf` is byte-identical.

The fixture pipeline (`tests/fixtures/run_sync_pipeline.py`) ingests
`carddav_tng`, copies these files over it, ingests again, and renders a
`diff` group between the two raw commits — one add, one delete, one
edit, so every row of the diff's table is exercised.
