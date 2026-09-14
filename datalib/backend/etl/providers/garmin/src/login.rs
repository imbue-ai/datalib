//! The Garmin SSO login the Connect phone app performs, ported from
//! garth: email + password (+ an emailed MFA code) → a service ticket →
//! a year-long OAuth1 token → the first bearer. Interactive by design;
//! nothing in the pipeline calls it.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::auth::{
    curl, curl_impersonated, exchange, oauth1_header, write_tokens, Nonce, OAuth1Token,
    OAuth2Token, OAUTH_USER_AGENT,
};

const CLIENT_ID: &str = "GCM_ANDROID_DARK";

/// The SSO pages sit behind Cloudflare's bot wall, so every request to
/// them goes out impersonated (Chrome's TLS fingerprint and user agent,
/// set by the dispatch curl) and with a browser's navigation headers.
const SSO_HEADERS: &[&str] = &[
    "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language: en-US,en;q=0.9",
    "Sec-Fetch-Mode: navigate",
    "Sec-Fetch-Dest: document",
];

/// Cloudflare's rate limit on the login endpoint fires on a credential
/// POST that follows the page load too quickly; the clients that get
/// through wait roughly this long between the two.
const PAUSE_BEFORE_CREDENTIALS: std::time::Duration = std::time::Duration::from_secs(35);

/// How the caller answers the two questions a login can ask. Split out
/// so the flow can be driven by a terminal or by a test.
pub trait Prompts {
    fn mfa_code(&mut self, method: &str) -> Result<String>;
    /// The login is about to wait this long before sending the
    /// credentials; a terminal says so, a test does not care.
    fn pausing(&mut self, _for: std::time::Duration) {}
}

pub struct TerminalPrompts;

impl Prompts for TerminalPrompts {
    fn mfa_code(&mut self, method: &str) -> Result<String> {
        read_line(&format!("MFA code (sent by {method}): "))
    }

    #[allow(clippy::disallowed_macros)]
    fn pausing(&mut self, d: std::time::Duration) {
        println!(
            "waiting {}s before submitting: Garmin's bot wall rate-limits a login that \
             follows the page load too quickly",
            d.as_secs()
        );
    }
}

pub struct LoginOutcome {
    pub oauth1: OAuth1Token,
    pub oauth2: OAuth2Token,
}

pub async fn login(
    domain: &str,
    email: &str,
    password: &str,
    prompts: &mut dyn Prompts,
) -> Result<LoginOutcome> {
    let jar = tempfile::NamedTempFile::new().context("cookie jar")?;
    let jar_path = jar.path().to_string_lossy().into_owned();
    let sso = format!("https://sso.{domain}");
    let service = format!("https://mobile.integration.{domain}/gcm/android");
    let login_query = format!(
        "clientId={CLIENT_ID}&locale=en-US&service={}",
        crate::auth::pct(&service)
    );

    // 1. The sign-in page, for the session cookies it sets.
    let mut args = vec!["-sS", "-c", &jar_path, "-b", &jar_path, "-o", "/dev/null"];
    for h in SSO_HEADERS {
        args.extend(["-H", h]);
    }
    args.extend(["-H", "Sec-Fetch-Site: none"]);
    let page = format!("{sso}/mobile/sso/en/sign-in?clientId={CLIENT_ID}");
    args.push(&page);
    let r = curl_impersonated(&args)
        .await
        .context("Garmin SSO sign-in page")?;
    if r.status != 200 {
        bail!("Garmin SSO sign-in page -> HTTP {}", r.status);
    }

    // 2. The credentials, after the pause the bot wall wants.
    prompts.pausing(PAUSE_BEFORE_CREDENTIALS);
    tokio::time::sleep(PAUSE_BEFORE_CREDENTIALS).await;
    let body = json!({
        "username": email,
        "password": password,
        "rememberMe": false,
        "captchaToken": "",
    });
    let reply = sso_post(
        &jar_path,
        &format!("{sso}/mobile/api/login?{login_query}"),
        &body,
    )
    .await
    .context("Garmin SSO login")?;
    let status_type = reply["responseStatus"]["type"]
        .as_str()
        .unwrap_or("UNKNOWN");
    let ticket = match status_type {
        "SUCCESSFUL" => ticket_of(&reply)?,
        "MFA_REQUIRED" => {
            let method = reply["customerMfaInfo"]["mfaLastMethodUsed"]
                .as_str()
                .unwrap_or("email")
                .to_string();
            let code = prompts.mfa_code(&method)?;
            let body = json!({
                "mfaMethod": method,
                "mfaVerificationCode": code.trim(),
                "rememberMyBrowser": false,
                "reconsentList": [],
                "mfaSetup": false,
            });
            let reply = sso_post(
                &jar_path,
                &format!("{sso}/mobile/api/mfa/verifyCode?{login_query}"),
                &body,
            )
            .await
            .context("Garmin SSO MFA")?;
            let t = reply["responseStatus"]["type"]
                .as_str()
                .unwrap_or("UNKNOWN");
            if t != "SUCCESSFUL" {
                bail!(
                    "Garmin SSO MFA: {t}: {}",
                    reply["responseStatus"]["message"].as_str().unwrap_or("")
                );
            }
            ticket_of(&reply)?
        }
        other => bail!(
            "Garmin SSO login: {other}: {}",
            reply["responseStatus"]["message"].as_str().unwrap_or("")
        ),
    };

    // 3. Best effort: the embed page sets a load-balancer cookie that
    // pins the next call to the backend that knows the ticket.
    let mut args = vec!["-sS", "-c", &jar_path, "-b", &jar_path, "-o", "/dev/null"];
    for h in SSO_HEADERS {
        args.extend(["-H", h]);
    }
    args.extend(["-H", "Sec-Fetch-Site: same-origin", "-H"]);
    let referer = format!("Referer: {sso}/mobile/sso/en/sign-in");
    args.push(&referer);
    let embed = format!("{sso}/portal/sso/embed");
    args.push(&embed);
    let _ = curl_impersonated(&args).await;

    // 4. Ticket → OAuth1 token. Signed with the consumer alone.
    let url = format!(
        "https://connectapi.{domain}/oauth-service/oauth/preauthorized?ticket={}&login-url={}&accepts-mfa-tokens=true",
        crate::auth::pct(&ticket),
        crate::auth::pct(&service),
    );
    let auth = oauth1_header("GET", &url, &[], None, &Nonce::fresh());
    let ua = format!("User-Agent: {OAUTH_USER_AGENT}");
    let auth_h = format!("Authorization: {auth}");
    let r = curl(&[
        "-sS", "-b", &jar_path, "-c", &jar_path, "-H", &ua, "-H", &auth_h, &url,
    ])
    .await
    .context("Garmin OAuth1 preauthorized")?;
    if r.status != 200 {
        bail!(
            "Garmin OAuth1 preauthorized -> HTTP {}: {}",
            r.status,
            r.body.chars().take(300).collect::<String>()
        );
    }
    let oauth1 = parse_oauth1(&r.body, domain)?;

    // 5. The first bearer.
    let oauth2 = exchange(&oauth1, true).await?;
    Ok(LoginOutcome { oauth1, oauth2 })
}

/// Run the whole thing against a terminal and leave the token files in
/// `dir`. `email` may be given up front; the password is always read
/// without echo.
pub async fn login_interactive(dir: &Path, domain: &str, email: Option<&str>) -> Result<()> {
    let email = match email {
        Some(e) => e.to_string(),
        None => read_line("Garmin email: ")?,
    };
    let password = read_secret("Garmin password: ")?;
    let out = login(domain, email.trim(), &password, &mut TerminalPrompts).await?;
    write_tokens(dir, Some(&out.oauth1), &out.oauth2)?;
    Ok(())
}

async fn sso_post(jar: &str, url: &str, body: &Value) -> Result<Value> {
    let payload = serde_json::to_string(body)?;
    let mut args = vec!["-sS", "-c", jar, "-b", jar, "-X", "POST"];
    for h in SSO_HEADERS {
        args.extend(["-H", h]);
    }
    args.extend([
        "-H",
        "Content-Type: application/json",
        "--data-binary",
        &payload,
        url,
    ]);
    let r = curl_impersonated(&args).await?;
    if r.status == 429 {
        bail!(
            "HTTP 429: Garmin's sign-in is rate-limiting this address. Wait fifteen minutes \
             or so before trying again; every attempt in the meantime extends the block"
        );
    }
    if r.status != 200 {
        bail!(
            "HTTP {}: {}",
            r.status,
            r.body.chars().take(300).collect::<String>()
        );
    }
    serde_json::from_str(&r.body)
        .with_context(|| format!("not JSON: {}", r.body.chars().take(200).collect::<String>()))
}

fn ticket_of(reply: &Value) -> Result<String> {
    reply["serviceTicketId"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Garmin SSO: login succeeded but no serviceTicketId in reply"))
}

/// The preauthorized reply is form-encoded, not JSON.
pub fn parse_oauth1(body: &str, domain: &str) -> Result<OAuth1Token> {
    let mut fields = std::collections::BTreeMap::new();
    for pair in body.trim().split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        fields.insert(
            k.to_string(),
            urlencoding::decode(v)
                .map(|c| c.into_owned())
                .unwrap_or_default(),
        );
    }
    let take = |k: &str| fields.get(k).cloned();
    Ok(OAuth1Token {
        oauth_token: take("oauth_token")
            .ok_or_else(|| anyhow!("preauthorized reply carries no oauth_token: {body:?}"))?,
        oauth_token_secret: take("oauth_token_secret")
            .ok_or_else(|| anyhow!("preauthorized reply carries no oauth_token_secret"))?,
        mfa_token: take("mfa_token").filter(|s| !s.is_empty()),
        mfa_expiration_timestamp: take("mfa_expiration_timestamp").filter(|s| !s.is_empty()),
        domain: Some(domain.to_string()),
    })
}

#[allow(clippy::disallowed_macros)]
fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Read a line with terminal echo off. `stty` rather than a crate: the
/// one caller is this interactive login, whose prompts are the point.
#[allow(clippy::disallowed_macros)]
fn read_secret(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let echo_off = std::process::Command::new("stty")
        .arg("-echo")
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if echo_off {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        println!();
    }
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preauthorized_reply_parses() {
        let t = parse_oauth1(
            "oauth_token=abc&oauth_token_secret=s%2Fs&mfa_token=m&mfa_expiration_timestamp=2027-03-18",
            "garmin.com",
        )
        .unwrap();
        assert_eq!(t.oauth_token, "abc");
        assert_eq!(t.oauth_token_secret, "s/s");
        assert_eq!(t.mfa_token.as_deref(), Some("m"));
        assert_eq!(t.domain.as_deref(), Some("garmin.com"));
        let no_mfa = parse_oauth1("oauth_token=a&oauth_token_secret=b", "garmin.cn").unwrap();
        assert_eq!(no_mfa.mfa_token, None);
        assert!(parse_oauth1("oauth_token_secret=b", "garmin.com").is_err());
    }
}
