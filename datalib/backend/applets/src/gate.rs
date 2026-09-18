//! What an applet requires of a request before answering it: the secret
//! the gateway handed it at spawn, and a `Host` naming the port it bound.
//!
//! The applet's port is loopback, but loopback is not private: any process
//! of the same user can connect, and a web page whose hostname resolves to
//! 127.0.0.1 can too. The gateway keeps its own routes behind the API
//! token; without this gate the applet would be a second, open door to
//! the same data. The names here mirror `datalib_http::applets`
//! (`ENV_APPLET_SECRET`, `APPLET_SECRET_HEADER`); the gateway round-trip
//! test is what keeps the two spellings together.

use anyhow::{Context, Result};

pub const ENV_SECRET: &str = "DATALIB_APPLET_SECRET";
/// Lower-case, since that is how axum and the hand-rolled parser both
/// present header names.
pub const SECRET_HEADER: &str = "x-datalib-applet-secret";

pub struct Gate {
    secret: String,
    /// `127.0.0.1:<port>`, the only `Host` a request may carry. A page
    /// reaching this port through its own hostname sends that hostname.
    host: String,
}

impl Gate {
    pub fn from_env(bound_port: u16) -> Result<Self> {
        let secret = std::env::var(ENV_SECRET)
            .ok()
            .filter(|s| !s.is_empty())
            .with_context(|| {
                format!(
                    "{ENV_SECRET} is not set. The gateway sets it when it starts an applet; \
                     to run one by hand, set it to any value and send that value in the \
                     {SECRET_HEADER} header of every request"
                )
            })?;
        Ok(Self {
            secret,
            host: format!("127.0.0.1:{bound_port}"),
        })
    }

    pub fn admits(&self, host: Option<&str>, secret: Option<&str>) -> bool {
        host == Some(self.host.as_str())
            && secret.is_some_and(|s| constant_time_eq(s, &self.secret))
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> Gate {
        Gate {
            secret: "s3cret".into(),
            host: "127.0.0.1:4321".into(),
        }
    }

    #[test]
    fn admits_only_the_gateways_own_host_and_secret() {
        let g = gate();
        assert!(g.admits(Some("127.0.0.1:4321"), Some("s3cret")));
        // The rebinding case: right secret would be impossible, but a
        // wrong host alone is enough to refuse.
        assert!(!g.admits(Some("evil.example:4321"), Some("s3cret")));
        assert!(!g.admits(Some("localhost:4321"), Some("s3cret")));
        assert!(!g.admits(Some("127.0.0.1:4321"), Some("s3cre")));
        assert!(!g.admits(Some("127.0.0.1:4321"), Some("s3cret ")));
        assert!(!g.admits(Some("127.0.0.1:4321"), None));
        assert!(!g.admits(None, Some("s3cret")));
    }
}
