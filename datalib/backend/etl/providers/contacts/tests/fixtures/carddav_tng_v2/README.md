# carddav_tng_v2 — the address books one sync later

`Bridge.vcf`, `Engineering.vcf` and `Borg.vcf` from `../carddav_tng/`, in
the same
flavors, after three changes to `Bridge.vcf`: Picard's card is **edited**
(a home phone added, the ship renamed to NCC-1701-E, `REV` bumped),
Data's card is **removed**, and Worf's card is **added**. The *Senior
Staff* group, `Engineering.vcf` and `Borg.vcf` are byte-identical.
`Maquis.vcf` is not here because it was **deleted**: the second sync
takes this folder as the whole set of address books, so both Maquis
contacts leave the store. `Borg.vcf` is here, unchanged, so its
Google-flavored cards stay in the rendered fixture.

The fixture pipeline (`tests/fixtures/run_sync_pipeline.py`) ingests
`carddav_tng`, makes its working copy match this folder, ingests again,
and renders a `diff` group between the two raw commits — one add, three
deletes (Data and the Maquis book) and one edit, so every row of the
diff's table is exercised.

The photos, Worf's included, are credited in `../carddav_tng/README.md`
§"The photos".
