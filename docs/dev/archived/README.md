# Archived docs

Point-in-time records kept for provenance — **not** current reference. They
describe the state of the codebase, or a plan/audit, at the time they were
written, and may reference layouts, APIs, or conventions that have since
changed. For the current design, see the live docs under `docs/dev/` (start
with [`data_architecture_ingestion.md`](../data_architecture_ingestion.md)).

| Doc | What it was |
|-----|-------------|
| [`data_architecture_audit.md`](data_architecture_audit.md) | ETL codebase audit produced 2026-06-09; several findings already marked superseded. |
| [`data_architecture_plan.md`](data_architecture_plan.md) | "Plan of attack" derived from that audit. Still cited by provider `schema_raw.rs` rustdocs as the rationale for the per-provider schema-file convention. |
| [`program_a_config_compose_plan.md`](program_a_config_compose_plan.md) | Step-by-step plan for the Program A config "compose, don't flatten" migration (issue #41), now landed. |
