//! GitLab REST API client (`gitlab.com/api/v4`).
//!
//! The client itself is [`datalib_etl_forge_common::ForgeClient`],
//! shared with github. GitLab signals rate limiting with a plain `429`,
//! which [`datalib_etl::http::default_retryability`] already handles, so
//! the only GitLab-specific thing left here is the API root.
//!
//! Port of `_call_gitlab_once` + `call_gitlab` + `paginate` in
//! `src/download/gitlab_web.py`.

use datalib_etl::http::default_retryability;
use datalib_etl_forge_common::ForgeClient;

pub use datalib_etl_forge_common::{ForgeError as GitLabError, LATCHKEY_TIMEOUT, PER_PAGE};

pub const BASE: &str = "https://gitlab.com/api/v4";

/// Alias kept so call sites read as GitLab code; the type is shared.
pub type GitLabClient = ForgeClient;

/// A [`ForgeClient`] wired to the `gitlab` latchkey service.
pub fn gitlab_client() -> ForgeClient {
    ForgeClient::new("gitlab", default_retryability)
}
