//! What every time-series render shares — yolink's and airvisual's
//! today. A provider's render crate keeps its own metric table (which
//! column plots where, in what unit), its own parse of its raw store,
//! and its own page; the Plotly page, the vocabulary those tables are
//! written in, the series type they produce and the number formatting
//! live here once.

pub mod plot;
pub mod series;
pub mod text;
pub mod units;
