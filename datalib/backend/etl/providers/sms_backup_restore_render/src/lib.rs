//! The render half of the `sms_backup_restore` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_sms_backup_restore`].

pub mod processor;
pub mod render;
