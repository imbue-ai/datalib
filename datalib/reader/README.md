# imbue-datalib-reader

Read the stores of a [datalib](https://github.com/imbue-ai/datalib) library
from Python. A datalib store is a doltlite file: SQLite with git-shaped history
kept inside it, which Python's own `sqlite3` module cannot open. This package
carries the doltlite library and a small module that calls it.

```sh
uv add imbue-datalib-reader==<version>     # or pip install; the version `datalib-dag --version` prints
```

```python
from datalib_reader import Store

path = "my-library/gmail/ingest/entities.doltlite_db"

with Store(path) as store:  # the latest sealed state
    rows = store.query("SELECT id, subject FROM emails WHERE subject LIKE ?", "%invoice%")
    commit = store.query("SELECT dolt_hashof('main')")[0][0]  # keep this to come back to

with Store(path, commit) as store:  # that commit, however far the store moves on
    count = store.query("SELECT count(*) FROM emails")

with Store(path) as store:  # after a later sync: what changed since that commit
    latest = store.query("SELECT dolt_hashof('main')")[0][0]
    changed = store.query("SELECT diff_type, to_id FROM dolt_diff_emails(?, ?)", commit, latest)
```

`query` returns a list of rows. A row is a tuple, so `row[0]` and unpacking
work, and it answers by column name too: `row["subject"]`, `row.as_dict()`.
The list's `columns` names the result's columns in order, even when no row came
back.

```python
from datalib_reader import Store, UnknownCommit, QueryError

try:
    with Store(path, commit) as store:
        rows = store.query("SELECT id, subject FROM emails LIMIT 5")
        print(rows.columns, [r["subject"] for r in rows])
except UnknownCommit:
    ...  # the store has no such commit: a `commit` kept from another store?
except QueryError:
    ...  # a syntax error, or a table or column this store does not have
```

Every failure is an `OSError` and a `datalib_reader.Error`, with SQLite's
extended result code in `code`. Each kind has its own class:

| class | when |
|---|---|
| `UnknownCommit` | `Store(path, commit)` named a commit or branch the store does not have |
| `CannotOpen` | no store at the path, or one this process may not read |
| `QueryError` | the SQL could not run: a syntax error, an unknown table or column |
| `ReadOnly` | the statement would write |
| `StoreBusy` | the store stayed locked past a five-second wait |

- **It only reads.** A write fails with `attempt to write a readonly database`,
  and the store comes out byte-identical.
- **It works under any Python 3.9 or later**, the builds `uv` fetches included.
  It starts no process, replaces no module and leaves no temp file, and
  `sqlite3` stays stock beside it.
- **Select a payload as `json(payload)`.** A datalib `payload` column is
  SQLite's binary JSON (JSONB); selected bare it comes back as `bytes`, which
  `json.loads` refuses with a `UnicodeDecodeError`.
- **Give each thread its own `Store`.**
- **Take the version of your datalib**, which `datalib-dag --version` prints.
  Each version carries the library that release's binaries were built from.

Wheels exist for Apple-silicon Macs on macOS 11 or later, and for Linux on x86_64
and aarch64: `manylinux_2_17` for any glibc distribution from the last decade, and
`musllinux_1_2` for Alpine. `pip` and `uv` pick the right one. There is no source
distribution, and no wheel for an Intel Mac.

What each table holds:
[Reading the raw stores](https://github.com/imbue-ai/datalib/blob/main/docs/user/raw_stores.md).
