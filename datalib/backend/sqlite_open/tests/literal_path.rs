//! A path that means something else to sqlx's URL parser or to SQLite still
//! names that path. One test in this binary, because it moves the process's
//! working directory to reach the names only a relative path can spell.

use std::path::Path;

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::Row;

async fn write_then_read_back(db: &Path) {
    let writer = pool(datalib_sqlite_open::file(db).create_if_missing(true)).await;
    sqlx::query("CREATE TABLE t (v TEXT)")
        .execute(&writer)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ('row')")
        .execute(&writer)
        .await
        .unwrap();
    writer.close().await;

    let reader = pool(datalib_sqlite_open::file(db).read_only(true)).await;
    let row = sqlx::query("SELECT v FROM t")
        .fetch_one(&reader)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>(0), "row");
    let refused = sqlx::query("INSERT INTO t VALUES ('another')")
        .execute(&reader)
        .await;
    assert!(refused.is_err(), "{} opened writable", db.display());
    reader.close().await;

    assert!(db.is_file(), "nothing at {}", db.display());
}

async fn pool(opts: sqlx::sqlite::SqliteConnectOptions) -> sqlx::SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .unwrap()
}

/// Guards the absolute path `sqlite://` decoded (`%41`) or cut (`?`) and the
/// relative names SQLite would read as a URI or as no file at all.
#[tokio::test]
async fn every_spelling_opens_the_file_it_names() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_current_dir(root.path()).unwrap();

    std::fs::create_dir(root.path().join("xAy")).unwrap();
    for dir in ["x%41y", "a?b", "c#d", "e@f", "x%41y?z#w@v"] {
        let dir = root.path().join(dir);
        std::fs::create_dir(&dir).unwrap();
        write_then_read_back(&dir.join("store.db")).await;
    }
    assert_eq!(
        std::fs::read_dir(root.path().join("xAy")).unwrap().count(),
        0,
        "a store went to the decoded path"
    );

    std::fs::create_dir(root.path().join("uri")).unwrap();
    std::fs::create_dir(root.path().join("file:uri")).unwrap();
    write_then_read_back(Path::new("file:uri/store.db")).await;
    assert!(!root.path().join("uri/store.db").exists());

    write_then_read_back(Path::new(":memory:")).await;
}
