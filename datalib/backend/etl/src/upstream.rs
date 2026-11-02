//! What a service's answer means for the run, said by the code that read
//! it. A provider puts an [`Upstream`] in its error's `source` chain where
//! it decides a request was refused, throttled or unanswered; the step's
//! failure classifier finds it there and never reads the words, so a
//! status spelled differently, or a 403 that means "quota", is not misread.

/// Why a service did not give us what we asked for, in the terms the
/// runner retries by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// The service would not take the credential, or latchkey would not
    /// send one. A person signs in again.
    CredentialRefused,
    /// latchkey's permissions, or its gateway's, do not allow this
    /// request. Signing in again changes nothing; granting it does.
    NotPermitted,
    /// The service is limiting how often we may ask.
    RateLimited,
    /// The service did not answer, or said it is briefly unavailable.
    Unavailable,
}

impl Upstream {
    /// What a bare status says, for a service whose statuses mean only
    /// what HTTP says they do.
    pub fn of_status(status: u16) -> Option<Self> {
        match status {
            401 | 403 => Some(Self::CredentialRefused),
            429 => Some(Self::RateLimited),
            502..=504 => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// An error saying `message`, caused by `why` when there is one: for
    /// a caller that keeps a failure as text and its meaning beside it.
    pub fn error(why: Option<Self>, message: String) -> anyhow::Error {
        match why {
            Some(why) => anyhow::Error::new(why).context(message),
            None => anyhow::Error::msg(message),
        }
    }

    /// The first one in `e`'s chain.
    pub fn in_chain(e: &anyhow::Error) -> Option<Self> {
        e.chain().find_map(|c| c.downcast_ref::<Self>().copied())
    }
}

impl std::fmt::Display for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CredentialRefused => "the service did not accept the credential",
            Self::NotPermitted => "latchkey's permissions do not allow this request",
            Self::RateLimited => "the service is limiting how often we may ask",
            Self::Unavailable => "the service did not answer",
        })
    }
}

impl std::error::Error for Upstream {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker is found however deep a provider buried it, and an
    /// error that carries none is not given one from its words.
    #[test]
    fn the_marker_is_read_from_the_chain_and_never_from_the_text() {
        let e = anyhow::Error::new(Upstream::RateLimited)
            .context("GET /me -> HTTP 403")
            .context("list orgs");
        assert_eq!(Upstream::in_chain(&e), Some(Upstream::RateLimited));
        let said = anyhow::anyhow!("GET /me -> HTTP 401 Unauthorized; rate limit; timed out");
        assert_eq!(Upstream::in_chain(&said), None);
    }
}
