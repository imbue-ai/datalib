# carddav_tng_v2 — the address books one sync later

`Bridge.vcf` and `Engineering.vcf` from `../carddav_tng/`, in the same
flavors, after three changes to `Bridge.vcf`: Picard's card is **edited**
(a home phone added, the ship renamed to NCC-1701-E, `REV` bumped),
Data's card is **removed**, and Worf's card is **added**. The *Senior
Staff* group and `Engineering.vcf` are byte-identical. `Borg.vcf` is not
here: only these files are laid over the first books, so it stays as it
was.

The fixture pipeline (`tests/fixtures/run_sync_pipeline.py`) ingests
`carddav_tng`, copies these files over it, ingests again, and renders a
`diff` group between the two raw commits — one add, one delete, one
edit, so every row of the diff's table is exercised.
