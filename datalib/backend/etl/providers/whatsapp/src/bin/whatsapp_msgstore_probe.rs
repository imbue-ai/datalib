//! Inventory a real `msgstore.db.crypt15`, and measure whether its rowids
//! survive from one backup to the next. This is the measurement behind the
//! question of whether `whatsapp` can sit on the generic SQLite mirror
//! engine the way `lightroom` and `apple_photos` do: that engine keys each
//! table on what the source declares, so it only produces sane diffs if
//! `_id` means the same row in consecutive backups.
//!
//! Usage: `WHATSAPP_BACKUP_DECRYPTION_KEY=… whatsapp_msgstore_probe <a.crypt15> [<b.crypt15>]`
//! With two files it joins `message`, `chat` and `jid` across them on their
//! natural keys and counts rows whose `_id` differs.
//! `--decrypt <in.crypt15> <out.db>` just writes the plaintext, for
//! pointing other tools (the mirror engine's CLI, `sqlite3`) at it.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use datalib_whatsapp_backup::{decode_hex_key, decrypt_file};

// Diagnostic tool; stdout is its whole output and nothing draws bars.
macro_rules! out {
    ($($t:tt)*) => {{
        #[allow(clippy::disallowed_macros)]
        { println!($($t)*) }
    }};
}

fn main() -> Result<()> {
    let key_hex = std::env::var("WHATSAPP_BACKUP_DECRYPTION_KEY")
        .context("WHATSAPP_BACKUP_DECRYPTION_KEY must be set")?;
    let key = decode_hex_key(&key_hex)?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, input, output] = args.as_slice() {
        if flag == "--decrypt" {
            let bytes =
                decrypt_file(Path::new(input), &key).with_context(|| format!("decrypt {input}"))?;
            std::fs::write(output, &bytes).with_context(|| format!("write {output}"))?;
            out!("{output}: {} bytes", bytes.len());
            return Ok(());
        }
    }
    let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    if paths.is_empty() || paths.len() > 2 {
        return Err(anyhow!(
            "usage: whatsapp_msgstore_probe <a.crypt15> [<b.crypt15>] | --decrypt <in.crypt15> <out.db>"
        ));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut plain = Vec::new();
        for p in &paths {
            let bytes =
                decrypt_file(p, &key).with_context(|| format!("decrypt {}", p.display()))?;
            let tmp = tempfile::Builder::new().suffix(".db").tempfile()?;
            std::fs::write(tmp.path(), &bytes)?;
            out!("== {} ({} bytes decrypted)", p.display(), bytes.len());
            let pool = open(tmp.path()).await?;
            inventory(&pool).await?;
            pool.close().await;
            plain.push(tmp);
        }
        if let [a, b] = plain.as_slice() {
            out!(
                "== rowid stability: {} -> {}",
                paths[0].display(),
                paths[1].display()
            );
            let pool = open(a.path()).await?;
            stability(&pool, b.path()).await?;
            pool.close().await;
        }
        Ok(())
    })
}

async fn open(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}

async fn inventory(pool: &SqlitePool) -> Result<()> {
    let kinds =
        sqlx::query("SELECT type, count(*) AS n FROM sqlite_master GROUP BY type ORDER BY type")
            .fetch_all(pool)
            .await?;
    for r in &kinds {
        out!(
            "  {:>8}: {}",
            r.get::<String, _>("type"),
            r.get::<i64, _>("n")
        );
    }

    let tables = sqlx::query(
        "SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    let mut autoinc = 0;
    let mut rowid_pk = 0;
    let mut composite_pk = 0;
    let mut has_unique = 0;
    let mut virtual_tables = Vec::new();
    let mut counted: Vec<(String, i64, String)> = Vec::new();
    for t in &tables {
        let name: String = t.get("name");
        let sql: String = t.get::<Option<String>, _>("sql").unwrap_or_default();
        if sql.contains("VIRTUAL TABLE") {
            virtual_tables.push(name);
            continue;
        }
        if sql.contains("AUTOINCREMENT") {
            autoinc += 1;
        }
        if sql.contains("UNIQUE") {
            has_unique += 1;
            out!("  UNIQUE in DDL: {name}");
        }
        let pk: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT name FROM pragma_table_info('{}') WHERE pk > 0 ORDER BY pk",
            name.replace('\'', "''")
        )))
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
        if pk.len() == 1 && pk[0] == "_id" {
            rowid_pk += 1;
        } else if pk.len() > 1 {
            composite_pk += 1;
        }
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM \"{}\"",
            name.replace('"', "\"\"")
        )))
        .fetch_one(pool)
        .await?;
        counted.push((name, n, pk.join(",")));
    }
    out!(
        "  tables: {} plain, {} virtual; pk=_id: {}, composite pk: {}, AUTOINCREMENT: {}, any UNIQUE: {}",
        counted.len(),
        virtual_tables.len(),
        rowid_pk,
        composite_pk,
        autoinc,
        has_unique
    );
    out!("  virtual: {}", virtual_tables.join(" "));
    let total: i64 = counted.iter().map(|c| c.1).sum();
    let nonempty = counted.iter().filter(|c| c.1 > 0).count();
    out!("  rows: {} across {} non-empty tables", total, nonempty);
    counted.sort_by_key(|c| std::cmp::Reverse(c.1));
    out!("  largest 25 (rows  pk  name):");
    for (name, n, pk) in counted.iter().take(25) {
        out!("    {:>9}  [{}]  {}", n, pk, name);
    }
    let idx_sql: Vec<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL ORDER BY tbl_name",
    )
    .fetch_all(pool)
    .await?;
    out!("  declared indexes:");
    for s in idx_sql {
        out!("    {s}");
    }
    Ok(())
}

async fn stability(pool: &SqlitePool, other: &Path) -> Result<()> {
    let uri = format!("file:{}?mode=ro", other.display());
    sqlx::query("ATTACH DATABASE ? AS b")
        .bind(uri)
        .execute(pool)
        .await
        .context("attach second msgstore")?;

    let checks: &[(&str, &'static str)] = &[
        (
            "jid",
            "SELECT count(*), sum(a._id != b._id) FROM main.jid a JOIN b.jid b ON a.raw_string = b.raw_string",
        ),
        (
            "chat",
            "SELECT count(*), sum(a._id != b._id)
               FROM main.chat a JOIN main.jid ja ON ja._id = a.jid_row_id
               JOIN b.jid jb ON jb.raw_string = ja.raw_string
               JOIN b.chat b ON b.jid_row_id = jb._id",
        ),
        (
            "message",
            "SELECT count(*), sum(a._id != b._id)
               FROM main.message a
               JOIN main.chat ca ON ca._id = a.chat_row_id
               JOIN main.jid ja ON ja._id = ca.jid_row_id
               JOIN b.jid jb ON jb.raw_string = ja.raw_string
               JOIN b.chat cb ON cb.jid_row_id = jb._id
               JOIN b.message b ON b.chat_row_id = cb._id AND b.key_id = a.key_id AND b.from_me = a.from_me",
        ),
    ];
    for (name, sql) in checks {
        let r = sqlx::query(*sql)
            .fetch_one(pool)
            .await
            .with_context(|| format!("stability {name}"))?;
        let matched: i64 = r.get(0);
        let renumbered: Option<i64> = r.get(1);
        out!(
            "  {:<8} matched by natural key: {:>8}   _id differs: {}",
            name,
            matched,
            renumbered.unwrap_or(0)
        );
    }
    let only_a: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM main.message a WHERE NOT EXISTS (
            SELECT 1 FROM b.message b JOIN b.chat cb ON cb._id = b.chat_row_id JOIN b.jid jb ON jb._id = cb.jid_row_id
             JOIN main.jid ja ON ja.raw_string = jb.raw_string JOIN main.chat ca ON ca.jid_row_id = ja._id
             WHERE ca._id = a.chat_row_id AND b.key_id = a.key_id AND b.from_me = a.from_me)",
    )
    .fetch_one(pool)
    .await?;
    let only_b: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM b.message b WHERE NOT EXISTS (
            SELECT 1 FROM main.message a JOIN main.chat ca ON ca._id = a.chat_row_id JOIN main.jid ja ON ja._id = ca.jid_row_id
             JOIN b.jid jb ON jb.raw_string = ja.raw_string JOIN b.chat cb ON cb.jid_row_id = jb._id
             WHERE cb._id = b.chat_row_id AND a.key_id = b.key_id AND a.from_me = b.from_me)",
    )
    .fetch_one(pool)
    .await?;
    out!("  message  only in first: {only_a}   only in second: {only_b}");
    Ok(())
}
