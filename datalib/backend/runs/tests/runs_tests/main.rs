//! Every hermetic run store test, one binary: each module is one
//! behaviour.

mod close_abandoned_invocations;
mod metric_reads;
mod reader_beside_a_writer;
mod request;
mod stock_sqlite_engine;
mod store_roundtrip;
