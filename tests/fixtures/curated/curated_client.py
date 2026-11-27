"""Calls the applets `serve_applets.sh` serves, as the app's buttons and an
agent call them: the contacts app's routes and the index's mute. The
curation scripts beside this file and in the live bake's data repo use it."""

import json
import struct
import urllib.error
import urllib.request
import zlib


class Applets:
    def __init__(self, state_dir):
        self.state = state_dir
        with open(f"{state_dir}/applet-secret") as f:
            self.secret = f.read().strip()

    def _port(self, app):
        with open(f"{self.state}/port.{app}") as f:
            return f.read().split("=")[1].strip()

    def call(self, app, method, path, body=None, raw=None, ctype="application/json"):
        data = (
            raw
            if raw is not None
            else (json.dumps(body).encode() if body is not None else None)
        )
        req = urllib.request.Request(
            f"http://127.0.0.1:{self._port(app)}{path}", data=data, method=method
        )
        req.add_header("X-Datalib-Applet-Secret", self.secret)
        if data is not None:
            req.add_header("Content-Type", ctype)
        try:
            with urllib.request.urlopen(req) as r:
                text = r.read().decode()
        except urllib.error.HTTPError as e:
            raise SystemExit(f"{method} {path}: {e.code} {e.read().decode()}") from e
        return json.loads(text) if text else {}

    def contacts(self, method, path, body=None, **kw):
        return self.call("datalib_contacts", method, path, body, **kw)

    def make(self, name, handles, kind=None):
        body = {"name": name, "handles": handles}
        if kind:
            body["kind"] = kind
        out = self.contacts("POST", "/contacts", body)
        print(f"made {name}: {out['contact_id']}")
        return out["contact_id"]

    def edit_fields(self, contact_id, fields, note=None):
        """Edit a contact's card as the card does: open its draft, autosave
        the whole edit, save naming the commit it was cut from."""
        draft = self.contacts("POST", f"/contact/{contact_id}/draft")
        mine = draft["mine"]
        mine["fields"] = fields
        if note is not None:
            mine["note"] = note
        self.contacts("PUT", f"/contact/{contact_id}/draft", mine)
        saved = self.contacts(
            "POST",
            f"/contact/{contact_id}/draft/save",
            {"seen": draft["published_commit"]},
        )
        assert saved["outcome"] == "saved", saved

    def mute(self, problem_uuids, agent=None):
        body = {"problem_uuids": problem_uuids, "muted": True}
        if agent:
            body["agent"] = agent
        return self.call(
            "unified_grid_and_search_index", "POST", "/problems/mute", body
        )


def png(w=8, h=8, rgb=(40, 90, 160)):
    """A small solid square, so a checked-in photo is nobody's picture."""
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))

    def chunk(t, d):
        return (
            struct.pack(">I", len(d))
            + t
            + d
            + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )
