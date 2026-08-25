//! IMAP connection: TLS, SASL, capability detection, and the raw FETCH
//! driver every phase of the download runs through.
//!
//! ## Why we drive FETCH by hand
//!
//! `async_imap::Session::uid_fetch` returns a stream of `Fetch`, whose
//! accessors cover most of what we need but not all of it: there is a
//! `gmail_labels()` and a `gmail_msg_id()`, but no `gmail_thr_id()`, and
//! `Fetch`'s inner response is private, so the thread id is unreachable
//! through that API. `uid_fetch` also can't express the CONDSTORE
//! `(CHANGEDSINCE n)` modifier that the incremental pass needs.
//!
//! So we issue the command with `run_command` and drain `read_response`
//! ourselves. `ResponseData::parsed()` is public and hands back the full
//! `imap_proto::Response`, which *does* model every attribute we care
//! about — UID, MODSEQ, RFC822.SIZE, FLAGS, INTERNALDATE, X-GM-MSGID,
//! X-GM-THRID, X-GM-LABELS, and the body section. One driver, uniform
//! access, no dependence on which accessors the wrapper happens to
//! expose.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_imap::imap_proto::{AttributeValue, Response, Status};
use async_imap::types::Capability;
use async_imap::{Authenticator, Client, Session};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::{client::TlsStream, TlsConnector};
use tracing::{debug, info};

use datalib_etl::latchkey::LatchkeyCredential;

/// The authenticated session type, spelled once.
pub type ImapSession = Session<TlsStream<TcpStream>>;

/// What the server told us it can do. Everything optional in the
/// download path keys off this rather than off the configured hostname,
/// so a non-Gmail server that happens to support an extension gets the
/// benefit and Gmail is not special-cased by name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// `X-GM-EXT-1` — Gmail's extensions: `X-GM-MSGID`, `X-GM-THRID`,
    /// `X-GM-LABELS`, `X-GM-RAW`.
    pub gmail: bool,
    /// RFC 7162 CONDSTORE — per-message MODSEQ and `(CHANGEDSINCE n)`,
    /// which is what makes the incremental flag/label pass cheap.
    pub condstore: bool,
    /// RFC 7162 QRESYNC — adds `VANISHED (EARLIER)`, i.e. the server
    /// tells us what was deleted. Gmail does **not** advertise this, so
    /// deletions there need a UID reconciliation sweep instead.
    pub qresync: bool,
    /// RFC 6154 SPECIAL-USE — `\All`, `\Sent`, `\Trash`… on LIST, which
    /// is how we find the all-mail folder without knowing the user's
    /// display language.
    pub special_use: bool,
    /// `COMPRESS=DEFLATE`. Mail compresses well and Gmail's IMAP cap is
    /// measured in bytes on the wire.
    pub compress: bool,
}

impl Caps {
    fn from_names<'a>(names: impl Iterator<Item = &'a str>) -> Self {
        let mut caps = Caps::default();
        for name in names {
            // IMAP capability names are case-insensitive (RFC 3501 §7.2.1).
            match name.to_ascii_uppercase().as_str() {
                "X-GM-EXT-1" => caps.gmail = true,
                "CONDSTORE" => caps.condstore = true,
                "QRESYNC" => caps.qresync = true,
                "SPECIAL-USE" => caps.special_use = true,
                "COMPRESS=DEFLATE" => caps.compress = true,
                _ => {}
            }
        }
        caps
    }
}

/// One FETCH response, flattened into owned values.
///
/// Every field is optional because which ones come back is a function of
/// what the command asked for: the metadata pass asks for no body, and
/// the incremental pass asks for neither body nor size.
#[derive(Debug, Clone, Default)]
pub struct FetchRow {
    pub uid: Option<u32>,
    pub modseq: Option<u64>,
    /// `RFC822.SIZE`. Read *before* the body is fetched, so it can drive
    /// the byte budget and the blob size cap prospectively rather than
    /// after the bytes are already on the wire.
    pub size: Option<u32>,
    pub flags: Vec<String>,
    pub internal_date: Option<String>,
    pub gm_msgid: Option<u64>,
    pub gm_thrid: Option<u64>,
    pub gm_labels: Vec<String>,
    /// The RFC 5322 source, when the command asked for `BODY.PEEK[]`.
    pub body: Option<Vec<u8>>,
}

impl FetchRow {
    fn from_attrs(attrs: &[AttributeValue<'_>]) -> Self {
        let mut row = FetchRow::default();
        for attr in attrs {
            match attr {
                AttributeValue::Uid(v) => row.uid = Some(*v),
                AttributeValue::ModSeq(v) => row.modseq = Some(*v),
                AttributeValue::Rfc822Size(v) => row.size = Some(*v),
                AttributeValue::InternalDate(v) => row.internal_date = Some(v.to_string()),
                AttributeValue::Flags(v) => {
                    row.flags = v.iter().map(|f| f.to_string()).collect();
                }
                AttributeValue::GmailMsgId(v) => row.gm_msgid = Some(*v),
                AttributeValue::GmailThrId(v) => row.gm_thrid = Some(*v),
                AttributeValue::GmailLabels(v) => {
                    row.gm_labels = v.iter().map(|l| l.to_string()).collect();
                }
                // `BODY.PEEK[]` comes back as `BODY[]`: a body section
                // with no path. A section *with* a path is a header-only
                // or part fetch, which we never ask for in the same
                // command as a full body, so taking only the pathless one
                // can't shadow anything.
                AttributeValue::BodySection {
                    section: None,
                    data: Some(d),
                    ..
                } => row.body = Some(d.to_vec()),
                AttributeValue::Rfc822(Some(d)) => row.body = Some(d.to_vec()),
                _ => {}
            }
        }
        row
    }
}

/// SASL `XOAUTH2` initial client response.
///
/// `base64("user=" <email> ^A "auth=Bearer " <token> ^A ^A)`, where `^A`
/// is `0x01`. async-imap does the base64 for us; `process` returns the
/// raw bytes.
struct XOAuth2 {
    user: String,
    token: String,
}

impl Authenticator for XOAuth2 {
    type Response = Vec<u8>;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token).into_bytes()
    }
}

/// A live, authenticated session plus what we learned establishing it.
pub struct Connected {
    pub session: ImapSession,
    pub caps: Caps,
    /// The account address we authenticated as. Callers default the
    /// `accounts` row's id / address to this so the credential itself
    /// never has to escape this module.
    pub username: String,
}

/// TLS-connect to `host:port` and authenticate with `cred`.
///
/// Implicit TLS only (RFC 8314) — there is no STARTTLS-on-143 path. Every
/// server worth mirroring offers 993, and negotiating up from cleartext
/// is a downgrade surface we have no reason to carry.
///
/// `address_hint` is the source's configured `imap.email_address`. A
/// `-u user:pass` credential carries the address itself and the hint is
/// only a fallback; an OAuth bearer token does not, so XOAUTH2 requires
/// the hint and fails clearly without it.
pub async fn connect(
    host: &str,
    port: u16,
    cred: &LatchkeyCredential,
    address_hint: Option<&str>,
) -> Result<Connected> {
    let tls = tls_connect(host, port).await?;
    let client = Client::new(tls);

    let (mut session, username) = match cred {
        LatchkeyCredential::Basic { username, password } => {
            let session = client
                .login(username, password)
                .await
                .map_err(|(e, _client)| auth_error(host, "LOGIN", e))?;
            (session, username.clone())
        }
        LatchkeyCredential::Bearer { token } => {
            // XOAUTH2 carries the account address inside the SASL blob,
            // so a bearer token alone cannot say *whose* mailbox to open.
            let user = address_hint.ok_or_else(|| {
                anyhow!(
                    "IMAP host {host}: the latchkey credential is an OAuth bearer token, which \
                     also needs the account address. Set `imap.email_address` on this source."
                )
            })?;
            let session = client
                .authenticate(
                    "XOAUTH2",
                    XOAuth2 {
                        user: user.to_string(),
                        token: token.clone(),
                    },
                )
                .await
                .map_err(|(e, _client)| auth_error(host, "XOAUTH2", e))?;
            (session, user.to_string())
        }
    };

    let caps = read_caps(&mut session).await?;
    info!(
        event = "imap_connected",
        host,
        port,
        gmail = caps.gmail,
        condstore = caps.condstore,
        qresync = caps.qresync,
        special_use = caps.special_use,
        "authenticated",
    );
    Ok(Connected {
        session,
        caps,
        username,
    })
}

async fn read_caps(session: &mut ImapSession) -> Result<Caps> {
    let caps = session.capabilities().await.context("IMAP CAPABILITY")?;
    Ok(Caps::from_names(caps.iter().map(capability_name)))
}

/// A capability is either the `IMAP4rev1` marker, an `AUTH=<mech>`, or a
/// bare atom; we want the name either way.
fn capability_name(cap: &Capability) -> &str {
    match cap {
        Capability::Imap4rev1 => "IMAP4REV1",
        Capability::Auth(s) | Capability::Atom(s) => s.as_str(),
    }
}

/// Turn a SASL failure into something a user can act on. Credential
/// values never appear — only the mechanism and the server's own text.
fn auth_error(host: &str, mechanism: &str, e: async_imap::error::Error) -> anyhow::Error {
    anyhow!(
        "IMAP {mechanism} to {host} was rejected: {e}. If this is Gmail with an app password, \
         confirm 2-Step Verification is on and that your Workspace admin has not disabled \
         app passwords; re-issue with `latchkey auth set <service> -u \"<user>:$(pbpaste)\"`."
    )
}

async fn tls_connect(host: &str, port: u16) -> Result<TlsStream<TcpStream>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // Name the provider explicitly rather than relying on
    // `ClientConfig::builder()`'s process-default. Cargo feature
    // unification across this workspace can leave rustls with more than
    // one provider compiled in, and the default-picking builder panics at
    // runtime when it can't choose — a failure mode that would only show
    // up on the first real connection.
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("rustls protocol versions")?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let server_name = ServerName::try_from(host.to_string())
        .with_context(|| format!("{host:?} is not a valid TLS server name"))?;
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    // Nagle costs us latency on the many small command round-trips an
    // IMAP session is made of, and buys nothing: we already batch.
    let _ = tcp.set_nodelay(true);
    TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}:{port}"))
}

/// Run one command and collect every untagged FETCH it produces, up to
/// the tagged completion.
///
/// Non-FETCH untagged responses (EXISTS, RECENT, unsolicited FLAGS…) are
/// discarded: the server is allowed to interleave them at any time, and
/// none of them affect what we store.
pub async fn fetch_raw(session: &mut ImapSession, command: &str) -> Result<Vec<FetchRow>> {
    debug!(event = "imap_fetch", command, "issuing");
    let tag = session
        .run_command(command)
        .await
        .with_context(|| format!("sending {command:?}"))?;

    let mut rows = Vec::new();
    loop {
        let Some(response) = session
            .read_response()
            .await
            .with_context(|| format!("reading response to {command:?}"))?
        else {
            bail!("IMAP connection closed while reading the response to {command:?}");
        };
        match response.parsed() {
            Response::Fetch(_seq, attrs) => rows.push(FetchRow::from_attrs(attrs)),
            Response::Done {
                tag: got,
                status,
                information,
                ..
            } if *got == tag => {
                return match status {
                    Status::Ok => Ok(rows),
                    _ => Err(anyhow!(
                        "IMAP {command:?} → {status:?}: {}",
                        information.as_deref().unwrap_or("(no detail)")
                    )),
                };
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_gmails_advertised_capabilities() {
        // Gmail's real CAPABILITY response.
        let caps = Caps::from_names(
            "IMAP4rev1 UNSELECT IDLE NAMESPACE QUOTA ID XLIST CHILDREN X-GM-EXT-1 UIDPLUS \
             COMPRESS=DEFLATE ENABLE MOVE CONDSTORE ESEARCH UTF8=ACCEPT LIST-EXTENDED \
             LIST-STATUS LITERAL- SPECIAL-USE APPENDLIMIT"
                .split_whitespace(),
        );
        assert!(caps.gmail);
        assert!(caps.condstore);
        assert!(caps.special_use);
        assert!(caps.compress);
        // Load-bearing: Gmail does NOT advertise QRESYNC, so there is no
        // VANISHED response and deletions need a reconciliation sweep.
        // If this ever flips, the deletion path can get much cheaper.
        assert!(!caps.qresync);
    }

    #[test]
    fn matches_capability_names_case_insensitively() {
        let caps = Caps::from_names("x-gm-ext-1 condstore special-use".split_whitespace());
        assert!(caps.gmail && caps.condstore && caps.special_use);
    }

    #[test]
    fn a_bare_server_advertises_nothing_we_key_off() {
        let caps = Caps::from_names("IMAP4rev1 STARTTLS AUTH=PLAIN".split_whitespace());
        assert_eq!(caps, Caps::default());
    }

    /// The SASL blob is `user=<addr>^Aauth=Bearer <tok>^A^A`. Getting the
    /// control characters wrong fails with an opaque server error, so pin
    /// the exact bytes.
    #[test]
    fn builds_the_xoauth2_initial_response() {
        let mut auth = XOAuth2 {
            user: "me@gmail.com".into(),
            token: "ya29.tok".into(),
        };
        assert_eq!(
            auth.process(b""),
            b"user=me@gmail.com\x01auth=Bearer ya29.tok\x01\x01".to_vec()
        );
    }

    fn attrs_body(data: &'static [u8]) -> Vec<AttributeValue<'static>> {
        vec![
            AttributeValue::Uid(42),
            AttributeValue::Rfc822Size(1234),
            AttributeValue::ModSeq(99),
            AttributeValue::BodySection {
                section: None,
                index: None,
                data: Some(data.into()),
            },
        ]
    }

    #[test]
    fn flattens_a_fetch_response() {
        let row = FetchRow::from_attrs(&attrs_body(b"From: a@b\r\n\r\nhi"));
        assert_eq!(row.uid, Some(42));
        assert_eq!(row.size, Some(1234));
        assert_eq!(row.modseq, Some(99));
        assert_eq!(row.body.as_deref(), Some(&b"From: a@b\r\n\r\nhi"[..]));
    }

    #[test]
    fn flattens_the_gmail_attributes() {
        let row = FetchRow::from_attrs(&[
            AttributeValue::GmailMsgId(0x1234),
            AttributeValue::GmailThrId(0x5678),
            AttributeValue::GmailLabels(vec!["\\Inbox".into(), "Work/Projects".into()]),
            AttributeValue::Flags(vec!["\\Seen".into()]),
        ]);
        assert_eq!(row.gm_msgid, Some(0x1234));
        // The reason we drive FETCH by hand: async-imap's `Fetch` has no
        // accessor for this one.
        assert_eq!(row.gm_thrid, Some(0x5678));
        assert_eq!(row.gm_labels, vec!["\\Inbox", "Work/Projects"]);
        assert_eq!(row.flags, vec!["\\Seen"]);
    }

    /// A metadata-only pass returns no body, and that must read as
    /// "not asked for", not as an empty message.
    #[test]
    fn reports_an_absent_body_as_none() {
        let row = FetchRow::from_attrs(&[AttributeValue::Uid(1)]);
        assert!(row.body.is_none());
    }
}
