"""Read a doltlite store from any Python, uv's included, by calling the libdoltlite
the imbue-datalib-reader wheel carries beside this file."""

import ctypes as c
import os
import sys
from pathlib import Path
from urllib.parse import quote

_name = "libdoltlite.dylib" if sys.platform == "darwin" else "libdoltlite.so"
_lib = c.CDLL(
    str(Path(__file__).parent / _name),
    mode=os.RTLD_NOW | os.RTLD_LOCAL | getattr(os, "RTLD_DEEPBIND", 0),
)

_ptr, _int, _bytes = c.c_void_p, c.c_int, c.c_char_p
for _fn, _res, _args in [
    ("sqlite3_open_v2", _int, [_bytes, c.POINTER(_ptr), _int, _bytes]),
    ("sqlite3_busy_timeout", _int, [_ptr, _int]),
    ("sqlite3_close_v2", _int, [_ptr]),
    ("sqlite3_errmsg", _bytes, [_ptr]),
    ("sqlite3_prepare_v2", _int, [_ptr, _bytes, _int, c.POINTER(_ptr), _ptr]),
    ("sqlite3_bind_null", _int, [_ptr, _int]),
    ("sqlite3_bind_int64", _int, [_ptr, _int, c.c_int64]),
    ("sqlite3_bind_double", _int, [_ptr, _int, c.c_double]),
    ("sqlite3_bind_text", _int, [_ptr, _int, _bytes, _int, _ptr]),
    ("sqlite3_bind_blob", _int, [_ptr, _int, _bytes, _int, _ptr]),
    ("sqlite3_step", _int, [_ptr]),
    ("sqlite3_finalize", _int, [_ptr]),
    ("sqlite3_column_count", _int, [_ptr]),
    ("sqlite3_column_type", _int, [_ptr, _int]),
    ("sqlite3_column_int64", c.c_int64, [_ptr, _int]),
    ("sqlite3_column_double", c.c_double, [_ptr, _int]),
    ("sqlite3_column_text", _ptr, [_ptr, _int]),
    ("sqlite3_column_blob", _ptr, [_ptr, _int]),
    ("sqlite3_column_bytes", _int, [_ptr, _int]),
]:
    getattr(_lib, _fn).restype, getattr(_lib, _fn).argtypes = _res, _args

_OPEN_READONLY, _OPEN_URI = 0x1, 0x40
_ROW, _DONE = 100, 101
_INTEGER, _FLOAT, _TEXT, _BLOB = 1, 2, 3, 4
_COPY_THE_VALUE = _ptr(-1)  # SQLITE_TRANSIENT


class Store:
    """One read-only connection: `main`, or the one commit it is given. Give
    each thread its own."""

    def __init__(self, path, commit=None):
        name = quote(str(path)) + (f"/{commit}" if commit else "")
        self._db = _ptr()
        rc = _lib.sqlite3_open_v2(
            f"file:{name}?mode=ro".encode(),
            c.byref(self._db),
            _OPEN_READONLY | _OPEN_URI,
            None,
        )
        if rc != 0:
            message = self._error()
            self.close()
            raise OSError(f"{path}: {message}")
        _lib.sqlite3_busy_timeout(self._db, 5000)  # what sqlite3.connect defaults to

    def query(self, sql, *params):
        stmt = _ptr()
        if _lib.sqlite3_prepare_v2(self._db, sql.encode(), -1, c.byref(stmt), None):
            raise OSError(self._error())
        try:
            for i, value in enumerate(params, 1):
                self._bind(stmt, i, value)
            rows = []
            while (rc := _lib.sqlite3_step(stmt)) == _ROW:
                columns = range(_lib.sqlite3_column_count(stmt))
                rows.append(tuple(_column(stmt, i) for i in columns))
            if rc != _DONE:
                raise OSError(self._error())
            return rows
        finally:
            _lib.sqlite3_finalize(stmt)

    def close(self):
        _lib.sqlite3_close_v2(self._db)
        self._db = _ptr()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def _error(self):
        return _lib.sqlite3_errmsg(self._db).decode()

    def _bind(self, stmt, i, value):
        if value is None:
            rc = _lib.sqlite3_bind_null(stmt, i)
        elif isinstance(value, int):
            rc = _lib.sqlite3_bind_int64(stmt, i, value)
        elif isinstance(value, float):
            rc = _lib.sqlite3_bind_double(stmt, i, value)
        elif isinstance(value, str):
            text = value.encode()
            rc = _lib.sqlite3_bind_text(stmt, i, text, len(text), _COPY_THE_VALUE)
        elif isinstance(value, bytes):
            rc = _lib.sqlite3_bind_blob(stmt, i, value, len(value), _COPY_THE_VALUE)
        else:
            raise TypeError(f"cannot bind {type(value).__name__}")
        if rc != 0:
            raise OSError(self._error())


def _column(stmt, i):
    kind = _lib.sqlite3_column_type(stmt, i)
    if kind == _INTEGER:
        return _lib.sqlite3_column_int64(stmt, i)
    if kind == _FLOAT:
        return _lib.sqlite3_column_double(stmt, i)
    if kind == _TEXT:
        pointer = _lib.sqlite3_column_text(stmt, i)
        return c.string_at(pointer, _lib.sqlite3_column_bytes(stmt, i)).decode()
    if kind == _BLOB:
        pointer = _lib.sqlite3_column_blob(stmt, i)
        size = _lib.sqlite3_column_bytes(stmt, i)
        return c.string_at(pointer, size) if size else b""
    return None
