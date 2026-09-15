//! `datalib_etl_airvisual`: the ingest side of the `airvisual` source —
//! an IQAir AirVisual Pro's own history files into a doltlite store.
//! The wire and the raw schema live under `ingest`; `processor` is what
//! `datalib-step` plans.

pub mod ingest;
pub mod processor;
