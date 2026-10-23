//! The blob store: the one part of a step's data that `datalib-http`
//! reads. Its shape is a contract between the step that writes it and
//! the server that hands its bytes out; README.md sets it out.

use std::path::Path;

use anyhow::{Context, Result};
use mail_parser::{MessageParser, MimeHeaders, PartType};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

pub use datalib_runtime::layout::BLOBS_DB;

/// The store's one table: bytes filed under their blake3, written once
/// and never changed — so no stamp either, since when a row arrived is
/// the date of the commit that brought it.
pub const CAS_OBJECTS_DDL: &str = "CREATE TABLE IF NOT EXISTS cas_objects (
    blake3            TEXT PRIMARY KEY,
    byte_len          INTEGER NOT NULL,
    content_type      TEXT NULL,
    bytes             BLOB NOT NULL,
    CHECK (length(blake3) = 64)
)";

/// How long a reader waits out a writer's commit. A commit writes bytes
/// already in memory, so it lasts as long as the disk write.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where a page finds the bytes `blake3` in the blob store of `step`
/// (`<group>/<function>`).
pub fn address(step: &str, blake3: &str) -> String {
    format!("/data/{step}/blob/{blake3}")
}

/// Where a page finds the MIME part whose decoded bytes hash to `part`,
/// inside the message stored under `message` in the blob store of `step`.
pub fn part_address(step: &str, message: &str, part: &str) -> String {
    format!("{}/part/{part}", address(step, message))
}

/// Whether `s` is a key the store could hold: a blake3 in lowercase hex.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The store at `path`, read-only. Nothing to pin: a reader sees
/// committed transactions only, and a row never changes once there.
pub async fn open_reader(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .read_only(true)
        .busy_timeout(BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open blob store {}", path.display()))
}

/// One row's bytes, and the type its writer stored with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

pub async fn get(pool: &SqlitePool, blake3: &str) -> Result<Option<Stored>> {
    let row = sqlx::query("SELECT content_type, bytes FROM cas_objects WHERE blake3 = ?")
        .bind(blake3)
        .fetch_optional(pool)
        .await
        .with_context(|| format!("read blob {blake3}"))?;
    row.map(|r| {
        Ok(Stored {
            content_type: r.try_get("content_type")?,
            bytes: r.try_get("bytes")?,
        })
    })
    .transpose()
}

/// One lookup in the store at `path`, opened and closed around it.
/// `None` when there is no store there, or it does not hold `blake3`.
pub async fn read(path: &Path, blake3: &str) -> Result<Option<Stored>> {
    if !path.is_file() {
        return Ok(None);
    }
    let pool = open_reader(path).await?;
    let got = get(&pool, blake3).await;
    pool.close().await;
    got
}

/// [`read`], then the part of that row whose decoded bytes hash to
/// `part`, when the row is a message. `None` when the store, the row or
/// the part is not there, or the row is not a message.
pub async fn read_part(path: &Path, message: &str, part: &str) -> Result<Option<Stored>> {
    Ok(read(path, message)
        .await?
        .and_then(|stored| part_of(&stored, part)))
}

/// The part of the message `stored` whose decoded bytes hash to `part`,
/// typed as the message says it is. Text comes out as UTF-8 whatever
/// charset it was sent in, and is typed so.
pub fn part_of(stored: &Stored, part: &str) -> Option<Stored> {
    let msg = MessageParser::default().parse(&stored.bytes)?;
    if !is_a_message(stored.content_type.as_deref(), !msg.headers().is_empty()) {
        return None;
    }
    msg.parts.iter().find_map(|p| {
        let declared = p.content_type().map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{sub}", ct.ctype()),
            None => ct.ctype().to_string(),
        });
        let (bytes, content_type) = match &p.body {
            PartType::Binary(b) | PartType::InlineBinary(b) => (b.as_ref(), declared),
            PartType::Text(t) => (t.as_bytes(), Some(utf8(declared, "text/plain"))),
            PartType::Html(t) => (t.as_bytes(), Some(utf8(declared, "text/html"))),
            PartType::Message(_) | PartType::Multipart(_) => return None,
        };
        (blake3::hash(bytes).to_hex().as_str() == part).then(|| Stored {
            content_type,
            bytes: bytes.to_vec(),
        })
    })
}

fn utf8(declared: Option<String>, otherwise: &str) -> String {
    format!(
        "{}; charset=utf-8",
        declared.as_deref().unwrap_or(otherwise)
    )
}

/// A row stored as `message/rfc822`, or stored with no type and reading
/// as a message with headers. A row its writer typed as anything else is
/// not one, however leniently the parser would read it.
fn is_a_message(content_type: Option<&str>, has_headers: bool) -> bool {
    match content_type {
        Some(ct) => ct
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("message/rfc822"),
        None => has_headers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store_with(path: &Path, rows: &[(&str, Option<&str>, &[u8])]) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query(CAS_OBJECTS_DDL).execute(&pool).await.unwrap();
        for (hash, ct, bytes) in rows {
            sqlx::query(
                "INSERT INTO cas_objects (blake3, byte_len, content_type, bytes) VALUES (?, ?, ?, ?)",
            )
            .bind(hash)
            .bind(bytes.len() as i64)
            .bind(ct)
            .bind(*bytes)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;
    }

    /// A row comes back with the type it was stored under; a hash the
    /// store lacks, or a step with no store at all, is `None` rather
    /// than an error, so the server can answer "not found".
    #[tokio::test]
    async fn reads_a_row_by_hash_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOBS_DB);
        let hash = blake3::hash(b"tea").to_hex().to_string();
        store_with(&path, &[(&hash, Some("text/plain"), b"tea")]).await;

        assert_eq!(
            read(&path, &hash).await.unwrap(),
            Some(Stored {
                content_type: Some("text/plain".into()),
                bytes: b"tea".to_vec()
            })
        );
        let other = blake3::hash(b"earl grey").to_hex().to_string();
        assert_eq!(read(&path, &other).await.unwrap(), None);
        assert_eq!(
            read(&dir.path().join("absent.sqlite"), &hash)
                .await
                .unwrap(),
            None
        );
    }

    const EML: &[u8] = b"From: Geordi La Forge <geordi@enterprise.example>\r\n\
To: Data <data@enterprise.example>\r\n\
Subject: Warp core schematic\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html; charset=iso-8859-1\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<p>Caf=E9</p>\r\n\
--b\r\n\
Content-Type: image/png\r\n\
Content-Disposition: attachment; filename=\"core.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--b--\r\n";

    fn message(content_type: Option<&str>, bytes: &[u8]) -> Stored {
        Stored {
            content_type: content_type.map(str::to_string),
            bytes: bytes.to_vec(),
        }
    }

    /// A part is found by the hash of its decoded bytes, not of the
    /// base64 or quoted-printable the message carries, and text comes out
    /// as UTF-8, typed so.
    #[test]
    fn a_part_comes_out_of_a_message_by_its_decoded_hash() {
        let eml = message(Some("message/rfc822"), EML);
        let png = b"\x89PNG\r\n\x1a\n";
        let image = part_of(&eml, &blake3::hash(png).to_hex()).expect("the image part");
        assert_eq!(image, message(Some("image/png"), png));

        let html = "<p>Caf\u{e9}</p>".as_bytes();
        let text = part_of(&eml, &blake3::hash(html).to_hex()).expect("the html part");
        assert_eq!(text, message(Some("text/html; charset=utf-8"), html));

        assert_eq!(part_of(&eml, &blake3::hash(b"never").to_hex()), None);
    }

    /// Only a row that is a message is taken apart: one stored as
    /// `message/rfc822`, or with no type and headers. A row typed as
    /// anything else is not, though it holds the same bytes.
    #[test]
    fn only_a_message_is_taken_apart() {
        let png = blake3::hash(b"\x89PNG\r\n\x1a\n").to_hex().to_string();
        assert!(part_of(&message(None, EML), &png).is_some());
        assert!(part_of(&message(Some("Message/RFC822; x=y"), EML), &png).is_some());
        assert_eq!(part_of(&message(Some("text/plain"), EML), &png), None);
        assert_eq!(part_of(&message(Some("application/pdf"), EML), &png), None);
    }

    #[test]
    fn a_part_address_follows_its_messages() {
        assert_eq!(
            part_address("gmail/ingest", "ab", "cd"),
            "/data/gmail/ingest/blob/ab/part/cd"
        );
    }

    #[test]
    fn a_hash_is_64_lowercase_hex_digits() {
        let hash = blake3::hash(b"tea").to_hex().to_string();
        assert!(is_hash(&hash));
        assert!(!is_hash(&hash.to_uppercase()));
        assert!(!is_hash(&hash[1..]));
        assert!(!is_hash(&format!("../{}", &hash[3..])));
    }

    #[test]
    fn an_address_names_the_step_then_the_hash() {
        assert_eq!(address("gmail/ingest", "ab"), "/data/gmail/ingest/blob/ab");
    }
}
