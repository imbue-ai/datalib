//! The `garmin` provider's ingest half: Garmin Connect, over the API
//! the Connect phone app uses. Render lives in `garmin_render`.

pub mod auth;
pub mod ingest;
pub mod login;
pub mod processor;
pub mod synthesize;
