//! What every time-series render shares — yolink's, airvisual's and
//! garmin's. A provider's render crate keeps its own metric table (which
//! column plots where, in what unit) and its own parse of its raw store;
//! the sensor page (`page`), the Plotly page, the vocabulary those
//! tables are written in, the series type they produce and the number
//! formatting live here once.

pub mod page;
pub mod plot;
pub mod series;
pub mod text;
pub mod units;
