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
| [`data_processor_and_config_refactor.md`](data_processor_and_config_refactor.md) | Program A: provider-owned config + `DataProcessor` trait + registry. Landed in full; still cited by the `*_config` crates' BUILD files for the config-crate convention (§4.3). |
| [`data_processor_pressure_test.md`](data_processor_pressure_test.md) | Program A's §5 on-paper pressure-test against the awkward providers (June 2026). |
| [`pipeline_dag_runner.md`](pipeline_dag_runner.md) | Program B plan: the processing DAG. Implemented as `datalib-dag`/`datalib-step` under different names — see [`pipeline_dag_architecture.md`](../pipeline_dag_architecture.md). |
| [`port_provider_to_signal_pattern.md`](port_provider_to_signal_pattern.md) | Per-provider recipe for the Signal/email raw-store pattern (anthropic, chatgpt, whatsapp — all ported). Superseded by [`provider_migration_dolt_diff_and_cas_edge.md`](../provider_migration_dolt_diff_and_cas_edge.md). |
| [`port_whatsapp_to_dolt_diff_incremental.md`](port_whatsapp_to_dolt_diff_incremental.md) | WhatsApp's CAS + `dolt_diff` incremental-render port plan, since landed (via the shared `render_cursor` variant). |
| [`google_takeout_ingestion.md`](google_takeout_ingestion.md) | Design draft for the Google Takeout provider (raw-extract-only scope); the built provider has since grown a render phase and a Google Voice feed. |
