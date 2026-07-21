//! `yolink-devices` — look up `deviceUDID`s via the official YoLink
//! cloud API. The UDID is one of the two values the downloader needs
//! to sign per-window CSV URLs (the other, `familyDeviceId`, is
//! captured separately from a download URL; see notes below).
//!
//! ## Why this CLI exists
//!
//! The downloader in `frankweiler_etl_yolink::download` builds signed
//! `us.yosmart.com/download/<family_device_id>/<sig>?...` URLs locally.
//! The signature is `md5(family_device_id + start_ms + end_ms +
//! device_udid)`. Both opaque IDs have to live in the per-device
//! `YolinkDevice` config (see `configs/thad_dev.yaml`).
//!
//! - `device_udid` (32-hex) — returned by the YoLink Open API as
//!   `deviceUDID`. This CLI exists to fetch them so the user doesn't
//!   have to reverse-engineer the app or scrape Android `logcat`
//!   for each device.
//! - `family_device_id` (32-hex) — **not in the API**. It's only
//!   visible on the wire when the app launches an intent to Chrome to
//!   download a CSV. To capture it, plug in your phone and:
//!     ```
//!     adb logcat | grep 'us\.yosmart\.com/download/'
//!     ```
//!   then export any chart from the YoLink/Safehous app. Each captured
//!   URL has the form `.../download/<family_device_id>/<sig>?...` —
//!   `<family_device_id>` is the first opaque path segment.
//!
//! ## Why this isn't enough on its own
//!
//! The Open API's `THSensor.getMetricsLogs` (which would give us full
//! historical data without any of this signing dance) is gated on
//! `DEVICE.HISTORICAL_DATA.READ`, a scope that is **not granted to
//! User Access Credentials** — only to CSID business-partner
//! credentials, and only after emailing yaochi@yosmart.com with a
//! stated purpose. UAC tokens can call `Home.getDeviceList` (this
//! CLI) and `*.getState` (current readings only). Historical data is
//! consumer-accessible only via the signed-URL endpoint we recreate.
//!
//! ## Usage
//!
//! Generate a UAC pair in the Safehous/YoLink app: Menu → Settings →
//! Account → Advanced Settings → User Access Credentials → "+". Then:
//!
//! ```
//! export YOLINK_UAID=ua_xxxxxxxxxxxxxxxxxxxxxxxxx
//! export YOLINK_SECRET_KEY=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
//! bazelisk run //frankweiler/backend/etl/providers/yolink:yolink_devices
//! ```
//!
//! Defaults to a human table. `--format yaml` emits a
//! `sources[name=yolink].sync.devices` block (with the
//! `family_device_id` field marked TODO so you can fill in those
//! values from your adb capture).

// This binary is a stdout-sink: the table / yaml / json output is its
// reason to exist, and there's no progress bar to corrupt. The NOTE
// banner goes to stderr so a redirected `> devices.yaml` stays clean.
#![allow(clippy::disallowed_macros)]

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::{json, Value};
use tokio::process::Command;

const TOKEN_URL: &str = "https://api.yosmart.com/open/yolink/token";
const API_URL: &str = "https://api.yosmart.com/open/yolink/v2/api";

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Table,
    Yaml,
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "yolink-devices",
    about = "List Home.getDeviceList output for the YoLink account behind \
             $YOLINK_UAID / $YOLINK_SECRET_KEY. Use the deviceUDID column to \
             populate `device_udid:` in YolinkDevice config."
)]
struct Args {
    /// YoLink UAC client id (created in the app under
    /// Settings → Account → Advanced Settings → User Access Credentials).
    #[arg(long, env = "YOLINK_UAID")]
    uaid: String,

    /// YoLink UAC secret key. Shown only once at creation time in
    /// the app; if you missed it, delete the UAC and make a new one.
    #[arg(long, env = "YOLINK_SECRET_KEY")]
    secret_key: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    format: Format,

    /// Filter by device type substring (e.g. `THSensor`,
    /// `WaterMeterController`). Case-insensitive.
    #[arg(long)]
    kind: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let token = exchange_token(&args.uaid, &args.secret_key).await?;
    let devices = list_devices(&token).await?;
    let filtered: Vec<&Value> = devices
        .iter()
        .filter(|d| match &args.kind {
            None => true,
            Some(k) => d
                .get("type")
                .and_then(Value::as_str)
                .map(|t| t.to_ascii_lowercase().contains(&k.to_ascii_lowercase()))
                .unwrap_or(false),
        })
        .collect();
    match args.format {
        Format::Table => print_table(&filtered),
        Format::Yaml => print_yaml(&filtered),
        Format::Json => print_json(&filtered)?,
    }
    Ok(())
}

async fn exchange_token(uaid: &str, secret: &str) -> Result<String> {
    let form = format!(
        "grant_type=client_credentials&client_id={uaid}&client_secret={secret}",
        uaid = urlencode(uaid),
        secret = urlencode(secret),
    );
    let body = post(
        TOKEN_URL,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        form.as_bytes(),
    )
    .await
    .context("POST /open/yolink/token")?;
    let v: Value = serde_json::from_slice(&body).context("token response JSON")?;
    let tok = v
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token response missing access_token: {v}"))?;
    Ok(tok.to_string())
}

async fn list_devices(token: &str) -> Result<Vec<Value>> {
    // `time` and `msgid` are Unix seconds for the YoLink API's
    // request envelope. Funnel through `frankweiler-time` so even
    // the binary's two-line `now()` honors the workspace policy.
    let now = IsoOffsetTimestamp::now_local().inner().timestamp();
    let body = json!({
        "method": "Home.getDeviceList",
        "time": now,
        "msgid": now.to_string(),
    });
    let auth = format!("Authorization: Bearer {token}");
    let resp_bytes = post(
        API_URL,
        &[("Content-Type", "application/json"), (auth.as_str(), "")],
        body.to_string().as_bytes(),
    )
    .await
    .context("POST /open/yolink/v2/api Home.getDeviceList")?;
    let v: Value = serde_json::from_slice(&resp_bytes).context("device-list JSON")?;
    let code = v.get("code").and_then(Value::as_str).unwrap_or("");
    if code != "000000" {
        bail!(
            "Home.getDeviceList failed: code={} desc={:?}",
            code,
            v.get("desc")
        );
    }
    let devices = v
        .get("data")
        .and_then(|d| d.get("devices"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("response missing data.devices: {v}"))?
        .clone();
    Ok(devices)
}

fn print_table(devices: &[&Value]) {
    println!(
        "{:<36}  {:<22}  {:<16}  {:<32}",
        "name", "type", "deviceId", "deviceUDID (→ device_udid)"
    );
    println!("{:-<36}  {:-<22}  {:-<16}  {:-<32}", "", "", "", "");
    for d in devices {
        let name = d.get("name").and_then(Value::as_str).unwrap_or("");
        let typ = d.get("type").and_then(Value::as_str).unwrap_or("");
        let did = d.get("deviceId").and_then(Value::as_str).unwrap_or("");
        let udid = d.get("deviceUDID").and_then(Value::as_str).unwrap_or("");
        println!("{name:<36}  {typ:<22}  {did:<16}  {udid:<32}");
    }
    eprintln!();
    eprintln!(
        "NOTE: this only gives you `device_udid:`. The matching \
         `family_device_id:` is not in the API — capture it by running \
         `adb logcat | grep us.yosmart.com/download/` while exporting \
         a CSV from the YoLink/Safehous app. See `yolink-devices --help`."
    );
}

fn print_yaml(devices: &[&Value]) {
    println!("# Generated by yolink-devices --format yaml.");
    println!("# Fill in family_device_id values from an adb logcat capture");
    println!("# of `us.yosmart.com/download/...` URLs while exporting a CSV");
    println!("# from the YoLink/Safehous app.");
    println!("devices:");
    for d in devices {
        let name = d.get("name").and_then(Value::as_str).unwrap_or("");
        let typ = d.get("type").and_then(Value::as_str).unwrap_or("");
        let udid = d.get("deviceUDID").and_then(Value::as_str).unwrap_or("");
        let kind = match typ {
            "THSensor" => "temperature_humidity",
            "WaterMeterController" => "watermeter",
            other => {
                // Surface as a comment so the user notices the unsupported kind.
                println!("  # {other:?} not handled by frankweiler-etl-yolink yet");
                continue;
            }
        };
        let slug = slugify(name);
        println!("  - name: {slug}");
        println!("    kind: {kind}");
        println!("    start: \"YYYY-MM-DD\"   # earliest date to backfill");
        println!("    family_device_id: \"<TODO from adb logcat capture>\"");
        println!("    device_udid: \"{udid}\"");
    }
}

fn print_json(devices: &[&Value]) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(devices)?);
    Ok(())
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if (c.is_whitespace() || c == '-' || c == '_') && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let is_safe =
            matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~');
        if is_safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Minimal `curl` POST wrapper — same approach as the extractor: shell
/// out so we don't introduce an http-client crate dep on this tiny
/// utility. Headers list passes `(name, value)`; an entry whose name
/// already starts with `<name>:` is sent as-is (used for the Bearer
/// header to keep the token off `argv` only after url-encoding).
async fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("curl");
    cmd.arg("-sSfL")
        .arg("-X")
        .arg("POST")
        .arg("--max-time")
        .arg("30")
        .arg("--data-binary")
        .arg("@-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in headers {
        let header = if v.is_empty() && k.contains(':') {
            k.to_string()
        } else {
            format!("{k}: {v}")
        };
        cmd.arg("-H").arg(header);
    }
    cmd.arg(url);
    let mut child = cmd.spawn().context("spawn curl")?;
    if let Some(stdin) = child.stdin.as_mut() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(body).await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .context("curl timeout")??;
    if !out.status.success() {
        bail!(
            "curl POST {url} exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}
