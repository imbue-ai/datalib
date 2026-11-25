#!/usr/bin/env python3
"""Generate the Discord fixture: `world.json`, everything Picard's account
can be answered, and the text files its messages carry.

A Discord id says when its record was made and cannot say a date after
2154, so this part of the corpus is in 2024: a temporal rift has left the
Enterprise in Earth's orbit that spring, and the crew use a chat service
of the period rather than subspace.

What it exists to cover:

  * **conversations**: a text, announcement, voice, stage, forum and media
    channel; a channel the account's role lets it read and one it may not;
    a channel type nothing reads; a public, private, announcement,
    archived and locked thread; a direct message with a person, a bot, a
    deleted account, and nobody's word in it yet; a group message with a
    name and one without; a server that is copied, a second with a channel
    of the same name, and one the config leaves out
  * **messages**: every type the render names, one it does not, replies to
    a message held and to one deleted, forwards, edits, a bot's embeds and
    laid-out text, a webhook, polls and their results, stickers, all of
    Discord's markdown, every kind of mention
  * **files**: photo, drawing, animation, video, voice message, music, PDF,
    archive, spreadsheet, code and a long paste; several on one message; a
    spoiler; a forwarded one; one over the size limit; one that is gone
  * **reactions**: the account's own, others', both; a custom emoji and an
    animated one; a burst

Field names are the live API's (2026-11-24) where the account probed had
the shape, and Discord's API reference's otherwise. Output is checked in:

    uv run python tests/fixtures/draw_discord_files.py     # only for new pictures
    uv run python tests/fixtures/make_discord_fixtures.py
"""

from __future__ import annotations

import hashlib
import io
import json
import zipfile
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

PROVIDERS = (
    Path(__file__).resolve().parents[2] / "datalib" / "backend" / "etl" / "providers"
)
OUT = PROVIDERS / "discord" / "tests" / "fixtures" / "discord_tng"
FILES = OUT / "files"

Obj = dict[str, Any]


def nulls(keys: str) -> Obj:
    return dict.fromkeys(keys.split())


# ── ids ──────────────────────────────────────────────────────────────

EPOCH_MS = 1_420_070_400_000
_taken: set[int] = set()


def snowflake(when: str) -> str:
    """For `05-01 09:00` or `05-01 09:00:30`, in 2024 and UTC."""
    stamp = f"2024-{when}" if when.count(":") == 2 else f"2024-{when}:00"
    at = datetime.strptime(stamp, "%Y-%m-%d %H:%M:%S").replace(tzinfo=UTC)
    snow = (int(at.timestamp() * 1000) - EPOCH_MS) << 22 | 1701
    while snow in _taken:
        snow += 1
    _taken.add(snow)
    return str(snow)


def iso(snow: str) -> str:
    stamp = datetime.fromtimestamp(((int(snow) >> 22) + EPOCH_MS) / 1000, UTC)
    return stamp.strftime("%Y-%m-%dT%H:%M:%S.%f+00:00")


def made(n: int) -> str:
    """An id for a record whose date nothing shows."""
    return str(170_100_000_000_000_000 + n)


def digest(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


# fmt: off

# ── people ───────────────────────────────────────────────────────────

PEOPLE: dict[str, Obj] = {}
NO_PROFILE = nulls(
    "banner accent_color avatar_decoration_data collectibles display_name_styles "
    "banner_color clan primary_guild"
)


def person(n: int, username: str, name: str | None, tag: str = "0") -> str:
    """A `tag` other than `0` is a discriminator, from before usernames were
    unique; a bot still has one."""
    uid = made(n)
    PEOPLE[uid] = NO_PROFILE | {
        "id": uid, "username": username, "avatar": digest(username)[:32], "discriminator": tag,
        "public_flags": 0, "flags": 0, "global_name": name,
    }
    return uid


def bot(n: int, username: str, tag: str) -> str:
    uid = person(n, username, None, tag)
    PEOPLE[uid] |= {"bot": True, "public_flags": 65536, "flags": 65536}
    return uid


PICARD = person(1, "jlpicard", "Jean-Luc Picard")
RIKER = person(2, "wtriker", "William T. Riker")
DATA = person(3, "soong_type", "Data")
TROI = person(4, "dtroi", "Deanna Troi")
WORF = person(5, "worf.of.mogh", "Worf")
LAFORGE = person(6, "geordi", "Geordi La Forge")
CRUSHER = person(7, "bevcrusher", "Beverly Crusher")
GUINAN = person(8, "guinan", "Guinan")
WESLEY = person(9, "wesley_c", "Wesley Crusher")
BARCLAY = person(10, "Broccoli", None, "0742")
OBRIEN = person(11, "transporter3", "Miles O'Brien")
COMPUTER = bot(12, "Ship's Computer", "1701")
Q = person(13, "q", "Q *the* <b>Omnipotent</b>")
LWAXANA = person(14, "lwaxana_troi", "Lwaxana Troi 💐")
DELETED = person(15, "deleted_user_47988", None)
BOOTHBY = person(16, "boothby", "Boothby")
MODERATOR = bot(17, "AutoMod", "0000")
for _faceless in (GUINAN, BARCLAY, DELETED):
    PEOPLE[_faceless]["avatar"] = None


def at(uid: str) -> str:
    return f"<@{uid}>"


# ── files ────────────────────────────────────────────────────────────

PICTURES = {
    ".jpg": "image/jpeg", ".png": "image/png", ".gif": "image/gif", ".webp": "image/webp",
    ".mp4": "video/mp4", ".mov": "video/quicktime",
}
TYPES = PICTURES | {
    ".ogg": "audio/ogg", ".mp3": "audio/mpeg", ".pdf": "application/pdf",
    ".zip": "application/zip", ".csv": "text/csv; charset=utf-8",
    ".lua": "text/x-lua; charset=utf-8", ".txt": "text/plain; charset=utf-8",
}
# A file's link is signed for the day the fixture is synced, whose "now"
# is 2369-04-15.
SIGNED = int(datetime(2369, 4, 15, tzinfo=UTC).timestamp())
# Link → the file it serves, or null where Discord no longer has it.
links: dict[str, str | None] = {}

ADDENDUM = """Chief engineer's note: these readings are from the aft array only.
The forward array is still pointed at whatever Q left there.
"""
LUA = """-- Phase variance across the aft array.
local function variance(readings)
  local sum, squares = 0, 0
  for _, r in ipairs(readings) do sum = sum + r end
  local mean = sum / #readings
  for _, r in ipairs(readings) do squares = squares + (r - mean) ^ 2 end
  return squares / #readings
end
"""


def text_files() -> dict[str, bytes]:
    csv = ["stardate,array,phase_variance"]
    csv += [f"47988.{n},aft,{0.012 * ((n * 7) % 11):.3f}" for n in range(1, 13)]
    flux = [(n, "nominal" if n % 9 else "ELEVATED") for n in range(1, 61)]
    paste = [f"[{n:03}] chroniton flux {is_} at deck {n % 42 + 1}" for n, is_ in flux]
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
        for name in ("tricorder/log.txt", "tricorder/README.txt"):
            z.writestr(zipfile.ZipInfo(name, (2024, 5, 1, 9, 0, 0)), ADDENDUM)
    log = PROVIDERS / "pdf" / "tests" / "fixtures" / "pdf_tng" / "captains_log.pdf"
    return {
        "aft_array.csv": ("\n".join(csv) + "\n").encode(), "phase_variance.lua": LUA.encode(),
        "message.txt": "\n".join(paste).encode(), "tricorder_dump.zip": archive.getvalue(),
        "captains_log.pdf": log.read_bytes(),
    }


def attach(when: str, channel: str, name: str, **more: Any) -> Obj:
    """A file on a message in `channel`. `size` names one that is not in
    `files/`: too large to fetch, or gone."""
    on_disk, suffix, aid = FILES / name, Path(name).suffix, snowflake(when)
    sign = f"?ex={SIGNED + 86_400:x}&is={SIGNED:x}&hm={digest(aid)}&"
    path = f"/attachments/{channel}/{aid}/{name}{sign}"
    url = f"https://cdn.discordapp.com{path}"
    links[url] = f"files/{name}" if on_disk.exists() else None
    out: Obj = {
        "id": aid, "filename": name, "size": more.pop("size", None) or on_disk.stat().st_size,
        "url": url, "proxy_url": f"https://media.discordapp.net{path}",
        "content_type": TYPES[suffix], "original_content_type": TYPES[suffix],
        "content_scan_version": 4,
    }
    if suffix in PICTURES:
        out |= {"width": 480, "height": 300}
        out |= {"placeholder": "BQgGBICXiQ==", "placeholder_version": 1}
    return out | more


# ── what a message may carry ─────────────────────────────────────────

DELTA: Obj = {"id": made(101), "name": "delta"}
SPIN: Obj = {"id": made(102), "name": "warp_spin", "animated": True}


def reacted(what: str | Obj, count: int, me: bool = False, burst: int = 0) -> Obj:
    return {
        "emoji": {"id": None, "name": what} if isinstance(what, str) else what, "count": count,
        "count_details": {"burst": burst, "normal": count - burst},
        "burst_colors": ["#ff3030"] if burst else [], "me_burst": False, "burst_me": False,
        "me": me, "burst_count": burst,
    }


def embed(kind: str = "rich", **fields: Any) -> Obj:
    return {"type": kind, **fields, "content_scan_version": 4}


def fields(**values: str) -> list[Obj]:
    return [{"name": n, "value": v, "inline": False} for n, v in values.items()]


def picture(url: str) -> Obj:
    return {"url": url, "width": 640, "height": 360}


def sticker(name: str, n: int) -> list[Obj]:
    return [{"id": made(200 + n), "name": name, "format_type": 1}]


def poll(question: str, expiry: str, mine: str = "", **answers: int) -> Obj:
    """Each answer's votes. Ended when the account voted for `mine`."""
    named = [(n, text.replace("_", " "), votes)
             for n, (text, votes) in enumerate(answers.items(), 1)]
    counts = [{"id": n, "count": v, "me_voted": t == mine} for n, t, v in named if v]
    return {
        "question": {"text": question},
        "answers": [{"answer_id": n, "poll_media": {"text": t}} for n, t, _ in named],
        "expiry": iso(snowflake(expiry)), "allow_multiselect": False, "layout_type": 1,
        "results": {"answer_counts": counts, "is_finalized": bool(mine)},
    }


# ── conversations ────────────────────────────────────────────────────

messages: dict[str, list[Obj]] = {}
threads: dict[str, list[Obj]] = {}
channels: dict[str, list[Obj]] = {}
dms: list[Obj] = []

ENTERPRISE, ACADEMY, FERENGI = map(snowflake, ["04-02 08:00", "04-02 08:30", "04-02 08:45"])
SENIOR, AWAY_TEAM, INTERNAL = map(snowflake, ["04-02 08:01", "04-02 08:02", "04-02 08:03"])

# May see a channel, and what was said in it.
READ = str(1 << 10 | 1 << 16)

CROSSPOSTED, HAS_THREAD = 1 << 0, 1 << 5
VOICE, FORWARD, LAID_OUT = 1 << 13, 1 << 14, 1 << 15

FORWARDED = (
    "type content mentions mention_roles attachments embeds timestamp "
    "edited_timestamp flags components"
)


class Room:
    """One conversation, its messages said in order."""

    def __init__(self, channel: Obj):
        self.channel = channel
        self.id: str = channel["id"]
        self.guild: str | None = channel.get("guild_id")
        self.said: list[Obj] = messages.setdefault(self.id, [])

    def say(self, when: str, who: str, content: str = "", *files: str, **more: Any) -> Obj:
        attachments = [attach(when, self.id, f) for f in files]
        mid = more.pop("id", None) or snowflake(when)
        named = [u for u in PEOPLE if at(u) in content] + more.pop("naming", [])
        roles = [r for r in (SENIOR, AWAY_TEAM, INTERNAL) if f"<@&{r}>" in content]
        edited = more.pop("edited", None)
        reactions = more.pop("reactions", None)
        out: Obj = {
            "type": 0, "content": content, "mentions": [PEOPLE[u] for u in named],
            "mention_roles": roles, "attachments": attachments, "embeds": [],
            "timestamp": iso(mid), "edited_timestamp": iso(snowflake(edited)) if edited else None,
            "flags": 0, "components": [], "id": mid, "channel_id": self.id, "author": PEOPLE[who],
            "pinned": False, "mention_everyone": "@everyone" in content, "tts": False,
        } | more
        if reactions:
            out["reactions"] = [reacted(*r) for r in reactions]
        self.said.append(out)
        self.channel["last_message_id"] = mid
        return out

    def ref(self, message: str | None, kind: int = 0, channel: str | None = None) -> Obj:
        out: Obj = {"type": kind, "channel_id": channel or self.id}
        out |= {"message_id": message} if message else {}
        return out | ({"guild_id": self.guild} if self.guild else {})

    def reply(self, when: str, who: str, to: Obj | str, content: str, **more: Any) -> Obj:
        """`to` is a message, or the id of one since deleted."""
        held = None if isinstance(to, str) else to
        reference = self.ref(to if isinstance(to, str) else to["id"])
        copy = held and {k: v for k, v in held.items() if k != "referenced_message"}
        more = {"type": 19} | more
        return self.say(when, who, content, message_reference=reference, referenced_message=copy, **more)

    def forward(self, when: str, who: str, source: Room, said: Obj, content: str = "") -> None:
        copy = {"message": {k: said[k] for k in FORWARDED.split()}}
        reference = source.ref(said["id"], kind=1)
        self.say(when, who, content, flags=FORWARD, message_reference=reference, message_snapshots=[copy])

    def thread(self, when: str, owner: str, name: str, kind: int = 11, off: Obj | None = None,
               *, archived: bool = False, locked: bool = False, **more: Any) -> Room:
        """A thread in this channel: off one of its messages, a forum's
        post, or on its own."""
        tid = off["id"] if off else snowflake(when)
        channel: Obj = {
            "id": tid,
            "type": kind,
            "last_message_id": tid,
            "flags": 0,
            "guild_id": self.guild,
            "name": name,
            "parent_id": self.id,
            "rate_limit_per_user": 0,
            "bitrate": 64000, "user_limit": 0, "rtc_region": None,
            "owner_id": owner,
            "thread_metadata": {
                "archived": archived,
                "archive_timestamp": iso(tid),
                "auto_archive_duration": 4320,
                "locked": locked,
                "create_timestamp": iso(tid),
            },
            "message_count": 0, "member_count": 3, "total_message_sent": 0,
            "member_ids_preview": [owner, PICARD],
        } | more
        threads.setdefault(self.id, []).append(channel)
        room = Room(channel)
        if off:
            off["thread"] = channel
            off["flags"] |= HAS_THREAD
            room.say(when, owner, type=21, message_reference=self.ref(off["id"]), referenced_message=None)
        return room

    def post(self, when: str, who: str, name: str, content: str, *files: str, **more: Any) -> Room:
        """A forum's post, whose first message has the post's own id."""
        room = self.thread(when, who, name, **more)
        room.say(when, who, content, *files, id=room.id)
        return room


def guild_channel(guild: str, when: str, kind: int, name: str, parent: Room | None = None,
                  **more: Any) -> Room:
    listed = channels.setdefault(guild, [])
    channel: Obj = {
        "id": snowflake(when), "type": kind, "last_message_id": None, "flags": 0,
        "guild_id": guild, "name": name, "parent_id": parent.id if parent else None,
        "rate_limit_per_user": 0, "topic": None, "position": len(listed),
        "permission_overwrites": [], "nsfw": False,
    } | more
    listed.append(channel)
    return Room(channel)


def dm(when: str, *people: str, group: str | None = None, kind: int = 1) -> Room:
    channel: Obj = {"id": snowflake(when), "type": kind, "last_message_id": None, "flags": 0}
    channel["recipients"] = [PEOPLE[p] for p in people]
    if kind == 3:
        channel |= {"name": group, "icon": None, "owner_id": people[0]}
    dms.append(channel)
    return Room(channel)


# ── the servers ──────────────────────────────────────────────────────


def guild(gid: str, name: str, features: list[str]) -> Obj:
    out = {"id": gid, "name": name, "icon": digest(name)[:32], "banner": None}
    return out | {"owner": False, "permissions": READ, "features": features}


def role(rid: str, gid: str, name: str) -> Obj:
    everyone = rid == gid
    colors = {"primary_color": 0} | nulls("secondary_color tertiary_color")
    return nulls("description icon unicode_emoji") | {
        "id": rid, "name": name, "permissions": READ if everyone else "0",
        "position": 0 if everyone else 1, "color": 0 if everyone else 0xC00000, "colors": colors,
        "hoist": False, "managed": False, "mentionable": True, "flags": 0,
    }


def membership(gid: str, roles: list[str], nick: str | None) -> Obj:
    unset = "avatar banner communication_disabled_until premium_since unusual_dm_activity_until"
    return nulls(unset + " collectibles display_name_styles") | {
        "flags": 0, "joined_at": iso(gid), "nick": nick, "roles": roles, "user": PEOPLE[PICARD],
        "bio": "", **dict.fromkeys(["pending", "mute", "deaf"], False),
    }


def only(allowed: str) -> list[Obj]:
    """A channel closed to everyone but one role."""
    closed = {"id": ENTERPRISE, "type": 0, "allow": "0", "deny": READ}
    return [closed, {"id": allowed, "type": 0, "allow": READ, "deny": "0"}]


# ── the story ────────────────────────────────────────────────────────

def enterprise() -> None:
    here = ENTERPRISE
    command = guild_channel(here, "04-02 08:10", 4, "Command")
    engineering = guild_channel(here, "04-02 08:11", 4, "Engineering")
    off_duty = guild_channel(here, "04-02 08:12", 4, "Off Duty")
    bridge = guild_channel(here, "04-02 08:13", 0, "bridge", command,
                           topic="Duty shifts and orders. Keep it in this century.")
    log = guild_channel(here, "04-02 08:14", 5, "captains-log", command)
    staff = guild_channel(here, "04-02 08:15", 0, "senior-staff", command,
                          permission_overwrites=only(SENIOR))
    # Listed to every member; nothing in it may be asked for.
    guild_channel(here, "04-02 08:16", 0, "internal-affairs", command,
                  permission_overwrites=only(INTERNAL), last_message_id=snowflake("05-02 07:00"))
    main = guild_channel(here, "04-02 08:17", 0, "main-engineering", engineering)
    tag = {"moderated": False, "emoji_id": None}
    tags = [tag | {"id": made(301), "name": "approved", "emoji_name": "✅"},
            tag | {"id": made(302), "name": "holodeck", "emoji_name": None}]
    proposals = guild_channel(here, "04-02 08:18", 15, "proposals", engineering, available_tags=tags,
                              default_sort_order=None, default_forum_layout=0,
                              default_reaction_emoji=None, template="")
    visual = guild_channel(here, "04-02 08:19", 16, "visual-logs", engineering, available_tags=[])
    forward = guild_channel(here, "04-02 08:20", 0, "ten-forward", off_duty)
    audio = {"bitrate": 64000, "user_limit": 4, "rtc_region": None}
    holodeck = guild_channel(here, "04-02 08:21", 2, "Holodeck 3", off_duty, **audio)
    theater = guild_channel(here, "04-02 08:22", 13, "Theater", off_duty, **audio)
    # A type the published reference does not list, as one server has.
    guild_channel(here, "04-02 08:23", 20, "briefing-canvas", command)

    on_the_bridge(bridge)
    in_the_log(log)
    among_senior_staff(staff, bridge)
    in_engineering(main)
    in_the_proposals(proposals)
    clip = visual.post("05-02 15:00", DATA, "The nebula, on approach",
                       "Two seconds of it, at twelve frames.", "nebula_approach.mp4")
    clip.say("05-02 15:04", GUINAN, "Prettier from the window.")
    in_ten_forward(forward, bridge)
    holodeck.say("05-03 20:00", BARCLAY, "Is anyone u-using the holodeck?")
    holodeck.say("05-03 20:01", LAFORGE, "All yours, Reg. **Safeties on.**")
    theater.say("05-03 19:00", DATA, "An Evening of Spot", type=27)
    theater.say("05-03 19:01", WESLEY, type=29)
    theater.say("05-03 19:10", DATA, "An Evening of Spot, abridged", type=31)
    theater.say("05-03 19:12", RIKER, "Bravo. *Is it over?*")
    theater.say("05-03 19:30", DATA, "An Evening of Spot, abridged", type=28)


def on_the_bridge(bridge: Room) -> None:
    for n, who in enumerate([RIKER, DATA, WORF]):
        bridge.say(f"04-28 08:0{n}", who, type=7)
    orders = bridge.say("05-01 09:00", PICARD,
        "## Standing orders\n"
        "We are in orbit of Earth, in **2024**. Until the rift is understood:\n"
        "1. No subspace traffic. *None.*\n"
        "2. No __technology__ leaves the ship.\n"
        f"3. {at(DATA)} has the calendar; {at(TROI)} has the crew.\n\n"
        "-# Temporal Prime Directive, section 7",
        pinned=True, edited="05-01 09:04", reactions=[("🫡", 6), (DELTA, 3, True)])
    bridge.say("05-01 09:01", PICARD, type=6, message_reference=bridge.ref(orders["id"]))
    bridge.reply("05-01 09:02", RIKER, orders, "Understood. Away teams are grounded.")
    bridge.say("05-01 09:05", DATA,
        "The local date is <t:1714554300:D>, a Wednesday. The rift opened <t:1714550400:R>.\n"
        "> Captain's query: can we return?\nThe probability is `0.0127`.")
    asked: Obj = {"type": 2, "name": "condition", "user": PEOPLE[WORF], "id": snowflake("05-01 09:06")}
    yellow = embed(title="Condition Yellow", description=f"All decks, stand by. Set by {at(WORF)}.",
        url="https://enterprise.starfleet.example/status", color=0xC0A000,
        timestamp="2024-05-01T09:06:00+00:00",
        author={"name": "Ship's Computer", "icon_url": "https://example.com/a.png"},
        fields=fields(**{"Shields": "**100%**", "Decks sealed": "4\n10\n36 to 38",
                         "Cause": "`temporal rift`, unexplained"}),
        footer={"text": "Voice authorization required"}, thumbnail=picture("https://example.com/t.png"))
    bridge.say("05-01 09:06", COMPUTER, type=20, embeds=[yellow], application_id=COMPUTER,
               interaction=asked, interaction_metadata=asked | {"command_type": 1})
    bridge.say("05-01 09:07", WORF, f"@everyone <@&{AWAY_TEAM}> to <#{bridge.id}>. Run "
               "</condition:170100000000000401> before you leave your station.")
    bridge.say("05-01 09:08", TROI, "I sense ~~fear~~ curiosity. ||And something very old.||",
               reactions=[("👀", 2, False, 1)])
    earth = attach("05-01 09:10", bridge.id, "earth_from_orbit.jpg", description="Earth's limb against the stars")
    photo = bridge.say("05-01 09:10", DATA,
        "Earth, from 400 kilometres. I have annotated nothing, as requested.",
        attachments=[earth], reactions=[("😍", 5, True), (SPIN, 1)])
    rift = bridge.thread("05-01 09:15", RIKER, "The temporal rift", off=photo)
    rift.say("05-01 09:16", LAFORGE, "Chroniton flux is climbing. Readings attached.",
             "aft_array.csv", "phase_variance.lua")
    rift.say("05-01 09:18", DATA, "```python\nflux = [r for r in readings if r > 0.1]\n```\n"
             "Three of twelve readings exceed the threshold.")
    rift.say("05-01 09:20", PICARD, f"Thank you. {at(LAFORGE)}, with me.")
    rota = bridge.thread("05-01 09:22", RIKER, "Shift rota")
    rota.say("05-01 09:23", WORF, "I will take every night shift.")
    bridge.say("05-01 09:22", RIKER, "Shift rota", type=18, message_reference=bridge.ref(None, channel=rota.id))
    question = "Where do we land the shuttle?"
    ballot = bridge.say("05-01 09:30", RIKER,
        poll=poll(question, "05-02 09:30", "Golden Gate Park", Golden_Gate_Park=5, Montana=2, La_Barre=1))
    result = fields(poll_question_text=question, victor_answer_votes="5", total_votes="8",
                    victor_answer_id="1", victor_answer_text="Golden Gate Park")
    bridge.say("05-02 09:30", RIKER, type=46, message_reference=bridge.ref(ballot["id"]),
               embeds=[embed("poll_result", fields=result)])
    relay = {"id": made(501), "username": "Sensor Relay 7", "discriminator": "0000"}
    bridge.say("05-02 10:00", COMPUTER, "Contact bearing 047 mark 8: one (1) weather balloon.",
               author=PEOPLE[COMPUTER] | relay, webhook_id=relay["id"], application_id=None)
    said = {"type": 10, "id": 5, "content": f"Alpha: {at(RIKER)}\nBeta: {at(DATA)}"}
    swap = {"type": 2, "id": 6, "style": 1, "label": "Swap", "custom_id": "swap"}
    roster = {"type": 17, "id": 1, "accent_color": None, "spoiler": False, "components": [
        {"type": 10, "id": 2, "content": "### Duty roster"},
        {"type": 14, "id": 3, "spacing": 1, "divider": True},
        {"type": 9, "id": 4, "components": [said], "accessory": swap}]}
    bridge.say("05-02 10:05", COMPUTER, flags=LAID_OUT, components=[roster])
    gone = snowflake("05-02 10:05:30")
    bridge.reply("05-02 10:06", WORF, gone, "That message is gone. I do not approve.")
    bridge.say("05-02 10:10", DATA,
        "Escapes, for the archive: \\*not bold\\*, <script>alert('no')</script>, "
        "| a | table |, #### four, ![x](https://example.com/x.png), 5 * 4 * 3, "
        "snake_case_name and C:\\holodeck\\3.")
    bridge.say("05-02 10:15", GUINAN,
        f"You should read <https://example.com/rift>, {at(PICARD)}, or "
        "[the short version](https://example.com/rift-short), or "
        "[this one](https://example.com/rift_(short)). See <id:guide> and https://example.com/a,b.")
    bridge.say("05-02 10:20", DATA, ">>> Tea,\nEarl Grey,\n- hot\n- in this century\n  - or any")
    bridge.say("05-02 10:25", Q, "Miss me, mon capitaine? <:delta:170100000000000101> "
               "<a:warp_spin:170100000000000102>", reactions=[("🙄", 7, True)])
    # A type no build knows yet: its text is shown, and a note is kept.
    bridge.say("05-02 10:26", Q, "*snap*", type=99)


def in_the_log(log: Room) -> None:
    entry = log.say("05-01 21:00", PICARD,
        "**Captain's log, supplemental.** The crew has taken to this century's habits "
        "with alarming ease. Mr. La Forge has discovered podcasts.",
        "captains_log.pdf", flags=CROSSPOSTED, reactions=[("📖", 4)])
    talk = log.thread("05-01 21:05", TROI, "Log: discussion", 10, off=entry)
    talk.say("05-01 21:06", TROI, "Shall we talk about the podcasts?")
    log.say("05-02 21:00", PICARD, flags=CROSSPOSTED, content=
        "The rift narrows by 3% each day. We leave within the month, or not at all.")


def among_senior_staff(staff: Room, bridge: Room) -> None:
    worry = staff.say("05-01 22:00", CRUSHER,
        "Eleven crew with hay fever. *Hay fever.* I had to look it up.", reactions=[("🤧", 3)])
    asked = staff.reply("05-01 22:02", RIKER, worry, "Can you treat it?")
    staff.reply("05-01 22:03", CRUSHER, asked,
                "With a hypospray? Yes. With anything on this planet? Not well.")
    orders = next(m for m in bridge.said if m["pinned"])
    staff.forward("05-01 22:10", RIKER, bridge, orders, "So nobody can claim they missed it.")
    photo = next(m for m in bridge.said if m["attachments"])
    staff.forward("05-01 22:12", TROI, bridge, photo)


def in_engineering(main: Room) -> None:
    main.say("05-01 11:00", LAFORGE, "Core's holding. Ignore the glow, the glow is *fine*.",
             "warp_core.webp", "red_alert.gif")
    main.say("05-01 11:05", LAFORGE, "Full flux log, it was too long to paste:",
             "message.txt", "tricorder_dump.zip")
    whole = attach("05-01 11:10", main.id, "holodeck_program_9.mov", size=48_000_000,
                   title="Holodeck Program 9 (full)")
    main.say("05-01 11:10", DATA, "The whole holodeck recording, for comparison.", attachments=[whole])
    lost = attach("05-01 11:12", main.id, "diagnostic_scan.png", size=8_192)
    main.say("05-01 11:12", BARCLAY, "I, uh, d-deleted the wrong file.", attachments=[lost])
    review = "https://example.com/starfleet/enterprise/pull/47"
    main.say("05-01 11:20", LAFORGE, review, embeds=[embed("article", url=review,
        title="Re-route the plasma conduits #47",
        description="Closes the loop that has been closing itself since stardate 45652.1. Reviewed by Data.",
        provider={"name": "Starfleet Forge"}, author={"name": "geordi"},
        thumbnail=picture("https://example.com/p.png"))])
    film = "https://example.com/watch?v=warp"
    main.say("05-01 11:25", WESLEY, film, embeds=[embed("video", url=film,
        title="Warp theory in ten minutes", provider={"name": "Period Video"},
        author={"name": "Z. Cochrane", "url": "https://example.com/zc"},
        video=picture("https://example.com/embed/warp"))])
    quiet = main.thread("05-01 12:00", LAFORGE, "Barclay's project", 12, invitable=False)
    quiet.say("05-01 12:01", BARCLAY, "P-please don't tell the others yet.")
    quiet.say("05-01 12:02", LAFORGE, "Not a word, Reg. It's good work.")
    old = main.thread("04-20 12:00", OBRIEN, "Transporter 3 calibration", archived=True)
    old.say("04-20 12:01", OBRIEN, "Pattern buffer's off by a hair. A *literal* hair.")
    old.say("04-20 12:30", LAFORGE, "Whose?")


def in_the_proposals(proposals: Room) -> None:
    cats = proposals.post("05-01 13:00", DATA, "A second litter box for Spot",
        "Spot's present accommodation is insufficient. Evidence attached.", "spot.png",
        applied_tags=[made(301)])
    cats.said[0]["reactions"] = [reacted("🐈", 12, True), reacted("👍", 9)]
    cats.say("05-01 13:10", WORF, "I will not be feeding it.", sticker_items=sticker("Glare", 1))
    cats.say("05-01 13:12", PICARD, "Approved. Make it so.")
    program = proposals.post("05-01 14:00", BARCLAY, "Three Musketeers, with fewer Musketeers",
        "A holodeck program in which nobody is d-demoted.", applied_tags=[made(302)])
    program.say("05-01 14:20", TROI, "Reg, shall we discuss this on Thursday?")
    closed = proposals.post("04-15 10:00", WESLEY, "Let me fly the ship",
        "Just the once. I have a diagram.", archived=True, locked=True, applied_tags=[])
    closed.say("04-15 10:05", RIKER, "No.", reactions=[("💀", 4)])


def in_ten_forward(forward: Room, bridge: Room) -> None:
    forward.say("05-01 19:00", RIKER, "Poker, 2100, my quarters. Bring replicator credits.",
                reactions=[("🃏", 4, True)])
    hand = forward.say("05-01 23:40", DATA,
        "A full house. I am told that I should have *concealed my satisfaction*.", "full_house.png")
    forward.reply("05-01 23:41", WORF, hand, "I demand a rematch.")
    forward.say("05-02 18:00", TROI, f"Nobody tell {at(PICARD)}!", "SPOILER_captains_cake.png",
                reactions=[("🎂", 8)])
    spoken = attach("05-02 18:05", forward.id, "voice-message.ogg", duration_secs=4.0,
                    waveform="AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRob")
    forward.say("05-02 18:05", RIKER, flags=VOICE, attachments=[spoken])
    forward.say("05-02 18:06", CRUSHER, "Was that a message or a trombone?",
                sticker_items=sticker("Facepalm", 2))
    forward.say("05-02 18:07", RIKER, "*This* is a trombone.", "trombone_practice.mp3")
    dance, homn = "https://example.com/gifs/dancing-valet", "https://example.com/mr-homn.png"
    forward.say("05-02 18:10", LWAXANA, dance, embeds=[embed("gifv", url=dance,
        provider={"name": "Period Gifs"}, video=picture("https://example.com/gifs/v.mp4"))])
    forward.say("05-02 18:12", LWAXANA, homn, embeds=[embed("image", url=homn, thumbnail=picture(homn))])
    forward.say("05-02 18:15", GUINAN, "日本語 · ελληνικά · עברית · tlhIngan Hol 🖖🏽👩‍🚀")
    forward.say("05-02 18:20", DELETED, "I was never here.")
    forward.say("05-02 18:30", WORF, "…", edited="05-02 18:31")
    ode = forward.say("05-03 08:00", DATA, sticker_items=sticker("Cat", 3), content=
        "# Ode to Spot\nFelis catus is your taxonomic nomenclature,\n"
        "an endothermic quadruped, carnivorous by nature.")
    forward.reply("05-03 08:05", LAFORGE, ode, "It's… long, Data.")
    forward.reply("05-03 08:06", COMPUTER, ode, "Translated: *meow.*", type=23, application_id=COMPUTER)
    date = next(m for m in bridge.said if "<t:" in m["content"])
    forward.forward("05-03 08:10", WESLEY, bridge, date)


def academy() -> None:
    sim = guild_channel(ACADEMY, "04-02 08:31", 0, "bridge", topic="Simulator bookings")
    grounds = guild_channel(ACADEMY, "04-02 08:32", 0, "grounds")
    sim.say("04-10 09:00", WESLEY, type=7)
    sim.say("04-10 09:01", WESLEY, "Kobayashi Maru, Tuesday. Who's in?")
    sim.say("04-10 09:02", WESLEY, type=8)
    sim.say("04-10 09:03", WESLEY, type=9)
    sim.say("04-10 09:04", BOOTHBY, "USS Enterprise #captains-log", type=12)
    sim.say("04-10 09:05", BOOTHBY, type=14)
    sim.say("04-10 09:06", COMPUTER, type=22)
    blocked = fields(rule_name="No commerce", channel_id=sim.id, decision_id="47988",
                     keyword="latinum", keyword_matched_content="latinum")
    sim.say("04-10 09:07", Q, type=24, embeds=[embed("auto_moderation_message",
        description="buy gold-pressed latinum at ferengi.example", fields=blocked)])
    sim.say("04-10 09:08", MODERATOR, type=36)
    sim.say("04-10 09:09", MODERATOR, type=37)
    sim.say("04-10 09:10", WESLEY, type=25)
    sim.say("04-10 09:11", WESLEY, type=32)
    sim.say("04-10 09:12", WESLEY, type=44)
    grounds.say("04-11 07:00", BOOTHBY, "The roses do not care what century it is.")
    grounds.say("04-11 07:30", PICARD, "I carved initials into an elm here, once. Or will.",
                reactions=[("🌳", 1)])


def direct() -> None:
    riker = dm("04-28 07:00", RIKER)
    riker.say("05-01 08:00", RIKER, "Ready when you are, sir.")
    riker.say("05-01 08:01", PICARD, "Number One, you have the bridge.", reactions=[("🖖", 1)])
    ended = iso(snowflake("05-01 08:20"))
    riker.say("05-01 08:02", RIKER, type=3, call={"participants": [RIKER, PICARD], "ended_timestamp": ended})
    riker.say("05-01 08:30", PICARD, "The chair suits you.", "earth_from_orbit.jpg")

    poker = dm("04-29 20:00", RIKER, DATA, WORF, group="Poker Night", kind=3)
    poker.say("04-29 20:00", RIKER, type=1, naming=[WORF])
    poker.say("04-29 20:01", RIKER, "Poker Night", type=4)
    poker.say("04-29 20:02", DATA, type=5)
    poker.say("04-29 20:05", LAFORGE, "Deal me out, shift until 0300.")
    poker.say("04-29 20:06", LAFORGE, type=2, naming=[LAFORGE])
    poker.say("04-29 20:07", RIKER, type=2, naming=[Q])
    poker.say("04-29 20:10", WORF, "Deuces are for children. And *no* Q.")
    poker.say("04-29 20:30", RIKER, poll=poll("Stakes?", "04-30 20:30", Chips=0, Shifts=3))

    surgery = dm("04-30 09:00", CRUSHER, TROI, kind=3)
    surgery.say("04-30 09:00", CRUSHER, "Captain, your physical is overdue.")
    surgery.say("04-30 09:01", TROI, "*Very* overdue.")
    surgery.say("04-30 09:30", PICARD, "I find that I am needed on the bridge.")

    guinan = dm("04-30 10:00", GUINAN)
    guinan.say("05-02 23:00", GUINAN, "Trust the rift, Picard. It's been here before.")
    guinan.say("05-02 23:05", PICARD, "Have you?")
    dm("04-30 11:00", DELETED).say("04-30 11:00", DELETED, "Do not go back through it.")
    usage = embed(title="Help", description="Say `locate`, then a name.")
    dm("04-30 12:00", COMPUTER).say("04-30 12:00", COMPUTER,
        "Please state the nature of your inquiry.", embeds=[usage])
    # Opened, and nothing said in it.
    dm("04-30 13:00", LWAXANA)


# fmt: on

# ── the file ─────────────────────────────────────────────────────────


def line(v: Any) -> str:
    return json.dumps(v, ensure_ascii=False)


def rows(v: list[Obj], indent: str) -> str:
    body = ",\n".join(f"{indent} {line(record)}" for record in v)
    return f"[\n{body}\n{indent}]" if v else "[]"


def part(key: str, value: Any) -> str:
    """One record to a line, so that a change to one message reads as one."""
    if isinstance(value, list):
        return rows(value, " ")
    if key == "me":
        return line(value)
    of = {k: rows(v, "  ") if isinstance(v, list) else line(v) for k, v in value.items()}  # fmt: skip
    return "{\n" + ",\n".join(f"  {line(k)}: {v}" for k, v in of.items()) + "\n }"


def main() -> None:
    FILES.mkdir(parents=True, exist_ok=True)
    for name, data in text_files().items():
        (FILES / name).write_bytes(data)
    enterprise()
    academy()
    direct()
    for thread in (t for listed in threads.values() for t in listed):
        said = len(messages[thread["id"]])
        thread["message_count"] = thread["total_message_sent"] = said
    me = PEOPLE[PICARD] | {
        "email": "picard@enterprise.starfleet",
        "phone": "+12025551701",
        "verified": True,
        "mfa_enabled": True,
        "locale": "en-GB",
        "premium_type": 0,
        "bio": "Tea. Earl Grey. Hot.",
        "linked_users": [],
        "authenticator_types": [2],
        "nsfw_allowed": True,
    }
    named = [(ENTERPRISE, "@everyone"), (SENIOR, "Senior Staff")]
    named += [(AWAY_TEAM, "Away Team"), (INTERNAL, "Internal Affairs")]
    world: Obj = {
        "me": me,
        "guilds": [
            guild(ENTERPRISE, "USS Enterprise", ["NEWS", "COMMUNITY"]),
            guild(ACADEMY, "Starfleet Academy", []),
            guild(FERENGI, "Ferengi Commerce Authority", ["COMMUNITY"]),
        ],
        "dms": dms,
        "channels": channels,
        "roles": {
            ENTERPRISE: [role(rid, ENTERPRISE, name) for rid, name in named],
            ACADEMY: [role(ACADEMY, ACADEMY, "@everyone")],
        },
        "members": {
            ENTERPRISE: membership(ENTERPRISE, [SENIOR, AWAY_TEAM], "Captain"),
            ACADEMY: membership(ACADEMY, [], None),
        },
        "threads": threads,
        "messages": messages,
        "files": links,
    }
    parts = ",\n".join(f" {line(k)}: {part(k, v)}" for k, v in world.items())
    (OUT / "world.json").write_text("{\n" + parts + "\n}\n", encoding="utf-8")
    print(f"{sum(map(len, messages.values()))} messages, {len(links)} files")


if __name__ == "__main__":
    main()
