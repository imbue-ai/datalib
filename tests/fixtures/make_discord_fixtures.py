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

# ── ids ──────────────────────────────────────────────────────────────

EPOCH_MS = 1_420_070_400_000


def instant(when: str) -> datetime:
    """`05-01 09:00` or `05-01 09:00:30`, in 2024 and UTC."""
    stamp = f"2024-{when}" if when.count(":") == 2 else f"2024-{when}:00"
    return datetime.strptime(stamp, "%Y-%m-%d %H:%M:%S").replace(tzinfo=UTC)


_taken: set[int] = set()


def snowflake(when: str) -> str:
    at = (int(instant(when).timestamp() * 1000) - EPOCH_MS) << 22 | 1701
    while at in _taken:
        at += 1
    _taken.add(at)
    return str(at)


def iso(snow: str) -> str:
    ms = (int(snow) >> 22) + EPOCH_MS
    stamp = datetime.fromtimestamp(ms / 1000, UTC)
    return stamp.strftime("%Y-%m-%dT%H:%M:%S.%f+00:00")


def digest(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


# ── people ───────────────────────────────────────────────────────────

PEOPLE: dict[str, Obj] = {}


def person(
    n: int,
    username: str,
    name: str | None,
    *,
    photo: bool = True,
    bot: bool = False,
    discriminator: str = "0",
) -> str:
    uid = str(170_100_000_000_000_000 + n)
    PEOPLE[uid] = {
        "id": uid,
        "username": username,
        "avatar": digest(username)[:32] if photo else None,
        "discriminator": discriminator,
        "public_flags": 65536 if bot else 0,
        "flags": 65536 if bot else 0,
        "banner": None,
        "accent_color": None,
        "global_name": name,
        "avatar_decoration_data": None,
        "collectibles": None,
        "display_name_styles": None,
        "banner_color": None,
        "clan": None,
        "primary_guild": None,
    } | ({"bot": True} if bot else {})
    return uid


PICARD = person(1, "jlpicard", "Jean-Luc Picard")
RIKER = person(2, "wtriker", "William T. Riker")
DATA = person(3, "soong_type", "Data")
TROI = person(4, "dtroi", "Deanna Troi")
WORF = person(5, "worf.of.mogh", "Worf")
LAFORGE = person(6, "geordi", "Geordi La Forge")
CRUSHER = person(7, "bevcrusher", "Beverly Crusher")
GUINAN = person(8, "guinan", "Guinan", photo=False)
WESLEY = person(9, "wesley_c", "Wesley Crusher")
# No display name, and a discriminator from before usernames were unique.
BARCLAY = person(10, "Broccoli", None, photo=False, discriminator="0742")
OBRIEN = person(11, "transporter3", "Miles O'Brien")
COMPUTER = person(12, "Ship's Computer", None, bot=True, discriminator="1701")
Q = person(13, "q", "Q *the* <b>Omnipotent</b>")
LWAXANA = person(14, "lwaxana_troi", "Lwaxana Troi 💐")
DELETED = person(15, "deleted_user_47988", None, photo=False)
BOOTHBY = person(16, "boothby", "Boothby")
MODERATOR = person(17, "AutoMod", None, bot=True, discriminator="0000")


def mention(uid: str) -> str:
    return f"<@{uid}>"


# ── files ────────────────────────────────────────────────────────────

TYPES = {
    ".jpg": "image/jpeg",
    ".png": "image/png",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".mp4": "video/mp4",
    ".mov": "video/quicktime",
    ".ogg": "audio/ogg",
    ".mp3": "audio/mpeg",
    ".pdf": "application/pdf",
    ".zip": "application/zip",
    ".csv": "text/csv; charset=utf-8",
    ".lua": "text/x-lua; charset=utf-8",
    ".txt": "text/plain; charset=utf-8",
}
PICTURES = {".jpg", ".png", ".gif", ".webp", ".mp4", ".mov"}
# A file's link is signed for the day the fixture is synced, whose "now"
# is 2369-04-15.
SIGNED = int(datetime(2369, 4, 15, tzinfo=UTC).timestamp())
# Link → the file it serves, or null where Discord no longer has it.
links: dict[str, str | None] = {}

ADDENDUM = """Chief engineer's note: these readings are from the aft array only.
The forward array is still pointed at whatever Q left there.
"""


def text_files() -> dict[str, bytes]:
    csv = ["stardate,array,phase_variance"]
    csv += [f"47988.{n},aft,{0.012 * ((n * 7) % 11):.3f}" for n in range(1, 13)]
    code = """-- Phase variance across the aft array.
local function variance(readings)
  local sum, squares = 0, 0
  for _, r in ipairs(readings) do sum = sum + r end
  local mean = sum / #readings
  for _, r in ipairs(readings) do squares = squares + (r - mean) ^ 2 end
  return squares / #readings
end
"""
    paste = [
        f"[{n:03}] chroniton flux {'nominal' if n % 9 else 'ELEVATED'} at deck {n % 42 + 1}"
        for n in range(1, 61)
    ]
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
        for name in ("tricorder/log.txt", "tricorder/README.txt"):
            z.writestr(zipfile.ZipInfo(name, (2024, 5, 1, 9, 0, 0)), ADDENDUM)
    log = PROVIDERS / "pdf" / "tests" / "fixtures" / "pdf_tng" / "captains_log.pdf"
    return {
        "aft_array.csv": ("\n".join(csv) + "\n").encode(),
        "phase_variance.lua": code.encode(),
        "message.txt": "\n".join(paste).encode(),
        "tricorder_dump.zip": archive.getvalue(),
        "captains_log.pdf": log.read_bytes(),
    }


def attach(when: str, channel: str, name: str, **more: Any) -> Obj:
    """A file on a message in `channel`. `size` names one that is not in
    `files/`: too large to fetch, or gone."""
    on_disk = FILES / name
    suffix = Path(name).suffix
    aid = snowflake(when)
    sign = f"?ex={SIGNED + 86_400:x}&is={SIGNED:x}&hm={digest(aid)}&"
    path = f"/attachments/{channel}/{aid}/{name}{sign}"
    url = f"https://cdn.discordapp.com{path}"
    out: Obj = {
        "id": aid,
        "filename": name,
        "size": more.pop("size", None) or on_disk.stat().st_size,
        "url": url,
        "proxy_url": f"https://media.discordapp.net{path}",
        "content_type": TYPES[suffix],
        "original_content_type": TYPES[suffix],
        "content_scan_version": 4,
    }
    if suffix in PICTURES:
        out["width"], out["height"] = 480, 300
        out["placeholder"], out["placeholder_version"] = "BQgGBICXiQ==", 1
    links[url] = f"files/{name}" if on_disk.exists() else None
    return out | more


# ── what a message may carry ─────────────────────────────────────────


def emoji(name: str, eid: str | None = None, animated: bool = False) -> Obj:
    out: Obj = {"id": eid, "name": name}
    return out | ({"animated": True} if animated else {})


DELTA = emoji("delta", "170100000000000101")
SPIN = emoji("warp_spin", "170100000000000102", animated=True)


def reacted(what: str | Obj, count: int, *, me: bool = False, burst: int = 0) -> Obj:
    return {
        "emoji": emoji(what) if isinstance(what, str) else what,
        "count": count,
        "count_details": {"burst": burst, "normal": count - burst},
        "burst_colors": ["#ff3030"] if burst else [],
        "me_burst": False,
        "burst_me": False,
        "me": me,
        "burst_count": burst,
    }


def embed(kind: str = "rich", **fields: Any) -> Obj:
    return {"type": kind, **fields, "content_scan_version": 4}


def field(name: str, value: str) -> Obj:
    return {"name": name, "value": value, "inline": False}


def picture(url: str) -> Obj:
    return {"url": url, "width": 640, "height": 360}


def sticker(name: str, n: int) -> Obj:
    return {"id": str(170_100_000_000_000_200 + n), "name": name, "format_type": 1}


def poll(
    question: str, answers: list[tuple[str, int, bool]], *, ended: bool, expiry: str
) -> Obj:
    return {
        "question": {"text": question},
        "answers": [
            {"answer_id": n, "poll_media": {"text": text}}
            for n, (text, _, _) in enumerate(answers, 1)
        ],
        "expiry": iso(snowflake(expiry)),
        "allow_multiselect": False,
        "layout_type": 1,
        "results": {
            "answer_counts": [
                {"id": n, "count": count, "me_voted": mine}
                for n, (_, count, mine) in enumerate(answers, 1)
                if count
            ],
            "is_finalized": ended,
        },
    }


# ── conversations ────────────────────────────────────────────────────

messages: dict[str, list[Obj]] = {}
threads: dict[str, list[Obj]] = {}
channels: dict[str, list[Obj]] = {}
dms: list[Obj] = []

ENTERPRISE = snowflake("04-02 08:00")
ACADEMY = snowflake("04-02 08:30")
FERENGI = snowflake("04-02 08:45")

SENIOR = snowflake("04-02 08:01")
AWAY_TEAM = snowflake("04-02 08:02")
INTERNAL = snowflake("04-02 08:03")

VIEW, HISTORY = 1 << 10, 1 << 16
READ = str(VIEW | HISTORY)

CROSSPOSTED, HAS_THREAD = 1 << 0, 1 << 5
VOICE, FORWARD, LAID_OUT = 1 << 13, 1 << 14, 1 << 15


class Room:
    """One conversation, its messages said in order."""

    def __init__(self, channel: Obj):
        self.channel = channel
        self.id: str = channel["id"]
        self.guild: str | None = channel.get("guild_id")
        self.said: list[Obj] = messages.setdefault(self.id, [])

    def say(self, when: str, who: str, content: str = "", **more: Any) -> Obj:
        mid = more.pop("id", None) or snowflake(when)
        named = [u for u in PEOPLE if mention(u) in content]
        named += more.pop("naming", [])
        edited = more.pop("edited", None)
        out: Obj = {
            "type": 0,
            "content": content,
            "mentions": [PEOPLE[u] for u in named],
            "mention_roles": [
                r for r in (SENIOR, AWAY_TEAM, INTERNAL) if f"<@&{r}>" in content
            ],
            "attachments": [],
            "embeds": [],
            "timestamp": iso(mid),
            "edited_timestamp": iso(snowflake(edited)) if edited else None,
            "flags": 0,
            "components": [],
            "id": mid,
            "channel_id": self.id,
            "author": PEOPLE[who],
            "pinned": False,
            "mention_everyone": "@everyone" in content or "@here" in content,
            "tts": False,
        } | more
        self.said.append(out)
        self.channel["last_message_id"] = mid
        return out

    def ref(
        self, message: str | None, channel: str | None = None, kind: int = 0
    ) -> Obj:
        out: Obj = {"type": kind, "channel_id": channel or self.id}
        if message:
            out["message_id"] = message
        return out | ({"guild_id": self.guild} if self.guild else {})

    def reply(
        self, when: str, who: str, to: Obj | str, content: str, **more: Any
    ) -> Obj:
        """`to` is a message, or the id of one since deleted."""
        if isinstance(to, str):
            answered, held = to, None
        else:
            answered = to["id"]
            held = {k: v for k, v in to.items() if k != "referenced_message"}
        reference = {
            "type": 19,
            "message_reference": self.ref(answered),
            "referenced_message": held,
        }
        return self.say(when, who, content, **(reference | more))

    def forward(
        self, when: str, who: str, source: Room, said: Obj, content: str = ""
    ) -> Obj:
        kept = ["type", "content", "mentions", "mention_roles", "attachments"]
        kept += ["embeds", "timestamp", "edited_timestamp", "flags", "components"]
        return self.say(
            when,
            who,
            content,
            flags=FORWARD,
            message_reference=source.ref(said["id"], kind=1),
            message_snapshots=[{"message": {k: said[k] for k in kept}}],
        )

    def thread(
        self,
        when: str,
        owner: str,
        name: str,
        kind: int = 11,
        *,
        off: Obj | None = None,
        archived: bool = False,
        locked: bool = False,
        **more: Any,
    ) -> Room:
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
            "bitrate": 64000,
            "user_limit": 0,
            "rtc_region": None,
            "owner_id": owner,
            "thread_metadata": {
                "archived": archived,
                "archive_timestamp": iso(tid),
                "auto_archive_duration": 4320,
                "locked": locked,
                "create_timestamp": iso(tid),
            },
            "message_count": 0,
            "member_count": 3,
            "total_message_sent": 0,
            "member_ids_preview": [owner, PICARD],
        } | more
        threads.setdefault(self.id, []).append(channel)
        room = Room(channel)
        if off:
            off["thread"] = channel
            off["flags"] |= HAS_THREAD
            started = self.ref(off["id"])
            room.say(
                when, owner, type=21, message_reference=started, referenced_message=None
            )
        return room

    def post(
        self, when: str, who: str, name: str, content: str, *files: str, **more: Any
    ) -> Room:
        """A forum's post, whose first message has the post's own id."""
        room = self.thread(when, who, name, **more)
        attachments = [attach(when, room.id, f) for f in files]
        room.say(when, who, content, id=room.id, attachments=attachments)
        return room


def guild_channel(
    guild: str, when: str, kind: int, name: str, parent: Room | None = None, **more: Any
) -> Room:
    listed = channels.setdefault(guild, [])
    channel: Obj = {
        "id": snowflake(when),
        "type": kind,
        "last_message_id": None,
        "flags": 0,
        "guild_id": guild,
        "name": name,
        "parent_id": parent.id if parent else None,
        "rate_limit_per_user": 0,
        "topic": None,
        "position": len(listed),
        "permission_overwrites": [],
        "nsfw": False,
    } | more
    listed.append(channel)
    return Room(channel)


def dm(when: str, kind: int, *people: str, name: str | None = None) -> Room:
    channel: Obj = {
        "id": snowflake(when),
        "type": kind,
        "last_message_id": None,
        "flags": 0,
        "recipients": [PEOPLE[p] for p in people],
    }
    if kind == 3:
        channel |= {"name": name, "icon": None, "owner_id": people[0]}
    dms.append(channel)
    return Room(channel)


# ── the servers ──────────────────────────────────────────────────────


def guild(gid: str, name: str, features: list[str]) -> Obj:
    return {
        "id": gid,
        "name": name,
        "icon": digest(name)[:32],
        "banner": None,
        "owner": False,
        "permissions": READ,
        "features": features,
    }


def role(rid: str, gid: str, name: str, permissions: str = "0") -> Obj:
    return {
        "id": rid,
        "name": name,
        "description": None,
        "permissions": permissions,
        "position": 0 if rid == gid else 1,
        "color": 0 if rid == gid else 0xC00000,
        "colors": {"primary_color": 0, "secondary_color": None, "tertiary_color": None},
        "hoist": False,
        "managed": False,
        "mentionable": True,
        "icon": None,
        "unicode_emoji": None,
        "flags": 0,
    }


def membership(gid: str, roles: list[str], nick: str | None) -> Obj:
    return {
        "avatar": None,
        "banner": None,
        "communication_disabled_until": None,
        "flags": 0,
        "joined_at": iso(gid),
        "nick": nick,
        "pending": False,
        "premium_since": None,
        "roles": roles,
        "unusual_dm_activity_until": None,
        "collectibles": None,
        "display_name_styles": None,
        "user": PEOPLE[PICARD],
        "mute": False,
        "deaf": False,
        "bio": "",
    }


def only(allowed: str) -> list[Obj]:
    """A channel closed to everyone but one role."""
    return [
        {"id": ENTERPRISE, "type": 0, "allow": "0", "deny": READ},
        {"id": allowed, "type": 0, "allow": READ, "deny": "0"},
    ]


def enterprise() -> None:
    here = ENTERPRISE
    command = guild_channel(here, "04-02 08:10", 4, "Command")
    engineering = guild_channel(here, "04-02 08:11", 4, "Engineering")
    off_duty = guild_channel(here, "04-02 08:12", 4, "Off Duty")
    topic = "Duty shifts and orders. Keep it in this century."
    bridge = guild_channel(here, "04-02 08:13", 0, "bridge", command, topic=topic)
    log = guild_channel(here, "04-02 08:14", 5, "captains-log", command)
    staff = guild_channel(
        here, "04-02 08:15", 0, "senior-staff", command,
        permission_overwrites=only(SENIOR),
    )  # fmt: skip
    # Listed to every member; nothing in it may be asked for.
    guild_channel(
        here, "04-02 08:16", 0, "internal-affairs", command,
        permission_overwrites=only(INTERNAL), last_message_id=snowflake("05-02 07:00"),
    )  # fmt: skip
    main = guild_channel(here, "04-02 08:17", 0, "main-engineering", engineering)
    tag = {"moderated": False, "emoji_id": None}
    proposals = guild_channel(
        here, "04-02 08:18", 15, "proposals", engineering,
        available_tags=[
            tag | {"id": "170100000000000301", "name": "approved", "emoji_name": "✅"},
            tag | {"id": "170100000000000302", "name": "holodeck", "emoji_name": None},
        ],
        default_sort_order=None, default_forum_layout=0, default_reaction_emoji=None,
        template="",
    )  # fmt: skip
    visual = guild_channel(
        here, "04-02 08:19", 16, "visual-logs", engineering, available_tags=[]
    )
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
    in_the_visual_logs(visual)
    in_ten_forward(forward, bridge)
    holodeck.say("05-03 20:00", BARCLAY, "Is anyone u-using the holodeck?")
    holodeck.say("05-03 20:01", LAFORGE, "All yours, Reg. **Safeties on.**")
    on_stage(theater)


def condition_yellow() -> Obj:
    return embed(
        title="Condition Yellow",
        description=f"All decks, stand by. Set by {mention(WORF)}.",
        url="https://enterprise.starfleet.example/status",
        color=0xC0A000,
        timestamp="2024-05-01T09:06:00+00:00",
        author={"name": "Ship's Computer", "icon_url": "https://example.com/a.png"},
        fields=[
            field("Shields", "**100%**"),
            field("Decks sealed", "4\n10\n36 to 38"),
            field("Cause", "`temporal rift`, unexplained"),
        ],
        footer={"text": "Voice authorization required"},
        thumbnail=picture("https://example.com/t.png"),
    )


def duty_roster() -> Obj:
    said = f"Alpha: {mention(RIKER)}\nBeta: {mention(DATA)}"
    swap = {"type": 2, "id": 6, "style": 1, "label": "Swap", "custom_id": "swap"}
    return {
        "type": 17,
        "id": 1,
        "accent_color": None,
        "spoiler": False,
        "components": [
            {"type": 10, "id": 2, "content": "### Duty roster"},
            {"type": 14, "id": 3, "spacing": 1, "divider": True},
            {
                "type": 9,
                "id": 4,
                "components": [{"type": 10, "id": 5, "content": said}],
                "accessory": swap,
            },
        ],
    }


def on_the_bridge(bridge: Room) -> None:
    at = bridge.id
    bridge.say("04-28 08:00", RIKER, type=7)
    bridge.say("04-28 08:01", DATA, type=7)
    bridge.say("04-28 08:02", WORF, type=7)
    orders = bridge.say(
        "05-01 09:00",
        PICARD,
        "## Standing orders\n"
        "We are in orbit of Earth, in **2024**. Until the rift is understood:\n"
        "1. No subspace traffic. *None.*\n"
        "2. No __technology__ leaves the ship.\n"
        f"3. {mention(DATA)} has the calendar; {mention(TROI)} has the crew.\n\n"
        "-# Temporal Prime Directive, section 7",
        pinned=True,
        edited="05-01 09:04",
        reactions=[reacted("🫡", 6), reacted(DELTA, 3, me=True)],
    )
    pinned = bridge.ref(orders["id"])
    bridge.say("05-01 09:01", PICARD, type=6, message_reference=pinned)
    bridge.reply("05-01 09:02", RIKER, orders, "Understood. Away teams are grounded.")
    bridge.say(
        "05-01 09:05",
        DATA,
        "The local date is <t:1714554300:D>, a Wednesday. The rift opened "
        "<t:1714550400:R>.\n> Captain's query: can we return?\n"
        "The probability is `0.0127`.",
    )
    asked = {"type": 2, "name": "condition", "user": PEOPLE[WORF]}
    asked["id"] = snowflake("05-01 09:06")
    bridge.say(
        "05-01 09:06",
        COMPUTER,
        type=20,
        embeds=[condition_yellow()],
        application_id=COMPUTER,
        interaction=asked,
        interaction_metadata=asked | {"command_type": 1},
    )
    bridge.say(
        "05-01 09:07",
        WORF,
        f"@everyone <@&{AWAY_TEAM}> to <#{at}>. Run "
        "</condition:170100000000000401> before you leave your station.",
    )
    bridge.say(
        "05-01 09:08",
        TROI,
        "I sense ~~fear~~ curiosity. ||And something very old.||",
        reactions=[reacted("👀", 2, burst=1)],
    )
    alt = "Earth's limb against the stars"
    photo = bridge.say(
        "05-01 09:10",
        DATA,
        "Earth, from 400 kilometres. I have annotated nothing, as requested.",
        attachments=[
            attach("05-01 09:10", at, "earth_from_orbit.jpg", description=alt)
        ],
        reactions=[reacted("😍", 5, me=True), reacted(SPIN, 1)],
    )
    the_rift(bridge.thread("05-01 09:15", RIKER, "The temporal rift", off=photo))
    rota = bridge.thread("05-01 09:22", RIKER, "Shift rota")
    rota.say("05-01 09:23", WORF, "I will take every night shift.")
    started = bridge.ref(None, rota.id)
    bridge.say("05-01 09:22", RIKER, rota.channel["name"], type=18, message_reference=started)  # fmt: skip
    where = [("Golden Gate Park", 5, True), ("Montana", 2, False), ("La Barre", 1, False)]  # fmt: skip
    question = "Where do we land the shuttle?"
    ballot = bridge.say(
        "05-01 09:30",
        RIKER,
        poll=poll(question, where, ended=True, expiry="05-02 09:30"),
    )
    result = [
        field("poll_question_text", question),
        field("victor_answer_votes", "5"),
        field("total_votes", "8"),
        field("victor_answer_id", "1"),
        field("victor_answer_text", "Golden Gate Park"),
    ]
    bridge.say(
        "05-02 09:30",
        RIKER,
        type=46,
        message_reference=bridge.ref(ballot["id"]),
        embeds=[embed("poll_result", fields=result)],
    )
    relay = "170100000000000501"
    bridge.say(
        "05-02 10:00",
        COMPUTER,
        "Contact bearing 047 mark 8: one (1) weather balloon.",
        author=PEOPLE[COMPUTER]
        | {"id": relay, "username": "Sensor Relay 7", "discriminator": "0000"},
        webhook_id=relay,
        application_id=None,
    )
    bridge.say("05-02 10:05", COMPUTER, flags=LAID_OUT, components=[duty_roster()])
    gone = snowflake("05-02 10:05:30")
    bridge.reply("05-02 10:06", WORF, gone, "That message is gone. I do not approve.")
    bridge.say(
        "05-02 10:10",
        DATA,
        "Escapes, for the archive: \\*not bold\\*, <script>alert('no')</script>, "
        "| a | table |, #### four, ![x](https://example.com/x.png), 5 * 4 * 3, "
        "snake_case_name and C:\\holodeck\\3.",
    )
    bridge.say(
        "05-02 10:15",
        GUINAN,
        f"You should read <https://example.com/rift>, {mention(PICARD)}, or "
        "[the short version](https://example.com/rift-short), or "
        "[this one](https://example.com/rift_(short)). See <id:guide> and "
        "https://example.com/a,b.",
    )
    bridge.say(
        "05-02 10:20",
        DATA,
        ">>> Tea,\nEarl Grey,\n- hot\n- in this century\n  - or any",
    )
    bridge.say(
        "05-02 10:25",
        Q,
        "Miss me, mon capitaine? <:delta:170100000000000101> "
        "<a:warp_spin:170100000000000102>",
        reactions=[reacted("🙄", 7, me=True)],
    )
    # A type no build knows yet: its text is shown, and a note is kept.
    bridge.say("05-02 10:26", Q, "*snap*", type=99)


def the_rift(rift: Room) -> None:
    rift.say(
        "05-01 09:16",
        LAFORGE,
        "Chroniton flux is climbing. Readings attached.",
        attachments=[
            attach("05-01 09:16", rift.id, "aft_array.csv"),
            attach("05-01 09:16", rift.id, "phase_variance.lua"),
        ],
    )
    rift.say(
        "05-01 09:18",
        DATA,
        "```python\nflux = [r for r in readings if r > 0.1]\n```\n"
        "Three of twelve readings exceed the threshold.",
    )
    rift.say("05-01 09:20", PICARD, f"Thank you. {mention(LAFORGE)}, with me.")


def in_the_log(log: Room) -> None:
    entry = log.say(
        "05-01 21:00",
        PICARD,
        "**Captain's log, supplemental.** The crew has taken to this century's habits "
        "with alarming ease. Mr. La Forge has discovered podcasts.",
        flags=CROSSPOSTED,
        attachments=[attach("05-01 21:00", log.id, "captains_log.pdf")],
        reactions=[reacted("📖", 4)],
    )
    talk = log.thread("05-01 21:05", TROI, "Log: discussion", 10, off=entry)
    talk.say("05-01 21:06", TROI, "Shall we talk about the podcasts?")
    log.say(
        "05-02 21:00",
        PICARD,
        "The rift narrows by 3% each day. We leave within the month, or not at all.",
        flags=CROSSPOSTED,
    )


def among_senior_staff(staff: Room, bridge: Room) -> None:
    worry = staff.say(
        "05-01 22:00",
        CRUSHER,
        "Eleven crew with hay fever. *Hay fever.* I had to look it up.",
        reactions=[reacted("🤧", 3)],
    )
    asked = staff.reply("05-01 22:02", RIKER, worry, "Can you treat it?")
    staff.reply(
        "05-01 22:03",
        CRUSHER,
        asked,
        "With a hypospray? Yes. With anything on this planet? Not well.",
    )
    orders = next(m for m in bridge.said if m["pinned"])
    staff.forward("05-01 22:10", RIKER, bridge, orders, "So nobody can claim they missed it.")  # fmt: skip
    photo = next(m for m in bridge.said if m["attachments"])
    staff.forward("05-01 22:12", TROI, bridge, photo)


def in_engineering(main: Room) -> None:
    at = main.id

    def send(when: str, who: str, content: str, *files: str, **more: Any) -> None:
        attachments = [attach(when, at, name, **more) for name in files]
        main.say(when, who, content, attachments=attachments)

    glow = "Core's holding. Ignore the glow, the glow is *fine*."
    send("05-01 11:00", LAFORGE, glow, "warp_core.webp", "red_alert.gif")
    paste = "Full flux log, it was too long to paste:"
    send("05-01 11:05", LAFORGE, paste, "message.txt", "tricorder_dump.zip")
    whole = "The whole holodeck recording, for comparison."
    title = "Holodeck Program 9 (full)"
    send("05-01 11:10", DATA, whole, "holodeck_program_9.mov", size=48_000_000, title=title)  # fmt: skip
    lost = "I, uh, d-deleted the wrong file."
    send("05-01 11:12", BARCLAY, lost, "diagnostic_scan.png", size=8_192)
    review = "https://example.com/starfleet/enterprise/pull/47"
    main.say(
        "05-01 11:20",
        LAFORGE,
        review,
        embeds=[
            embed(
                "article",
                url=review,
                title="Re-route the plasma conduits #47",
                description="Closes the loop that has been closing itself since "
                "stardate 45652.1. Reviewed by Data.",
                provider={"name": "Starfleet Forge"},
                author={"name": "geordi"},
                thumbnail=picture("https://example.com/p.png"),
            )
        ],
    )
    film = "https://example.com/watch?v=warp"
    main.say(
        "05-01 11:25",
        WESLEY,
        film,
        embeds=[
            embed(
                "video",
                url=film,
                title="Warp theory in ten minutes",
                provider={"name": "Period Video"},
                author={"name": "Z. Cochrane", "url": "https://example.com/zc"},
                video=picture("https://example.com/embed/warp"),
            )
        ],
    )
    quiet = main.thread(
        "05-01 12:00", LAFORGE, "Barclay's project", 12, invitable=False
    )
    quiet.say("05-01 12:01", BARCLAY, "P-please don't tell the others yet.")
    quiet.say("05-01 12:02", LAFORGE, "Not a word, Reg. It's good work.")
    old = main.thread("04-20 12:00", OBRIEN, "Transporter 3 calibration", archived=True)
    old.say("04-20 12:01", OBRIEN, "Pattern buffer's off by a hair. A *literal* hair.")
    old.say("04-20 12:30", LAFORGE, "Whose?")


def in_the_proposals(proposals: Room) -> None:
    cats = proposals.post(
        "05-01 13:00",
        DATA,
        "A second litter box for Spot",
        "Spot's present accommodation is insufficient. Evidence attached.",
        "spot.png",
        applied_tags=["170100000000000301"],
    )
    cats.said[0]["reactions"] = [reacted("🐈", 12, me=True), reacted("👍", 9)]
    glare = [sticker("Glare", 1)]
    cats.say("05-01 13:10", WORF, "I will not be feeding it.", sticker_items=glare)
    cats.say("05-01 13:12", PICARD, "Approved. Make it so.")
    program = proposals.post(
        "05-01 14:00",
        BARCLAY,
        "Three Musketeers, with fewer Musketeers",
        "A holodeck program in which nobody is d-demoted.",
        applied_tags=["170100000000000302"],
    )
    program.say("05-01 14:20", TROI, "Reg, shall we discuss this on Thursday?")
    closed = proposals.post(
        "04-15 10:00",
        WESLEY,
        "Let me fly the ship",
        "Just the once. I have a diagram.",
        archived=True,
        locked=True,
        applied_tags=[],
    )
    closed.say("04-15 10:05", RIKER, "No.", reactions=[reacted("💀", 4)])


def in_the_visual_logs(visual: Room) -> None:
    clip = visual.post(
        "05-02 15:00",
        DATA,
        "The nebula, on approach",
        "Two seconds of it, at twelve frames.",
        "nebula_approach.mp4",
    )
    clip.say("05-02 15:04", GUINAN, "Prettier from the window.")


def in_ten_forward(forward: Room, bridge: Room) -> None:
    at = forward.id
    game = "Poker, 2100, my quarters. Bring replicator credits."
    forward.say("05-01 19:00", RIKER, game, reactions=[reacted("🃏", 4, me=True)])
    hand = forward.say(
        "05-01 23:40",
        DATA,
        "A full house. I am told that I should have *concealed my satisfaction*.",
        attachments=[attach("05-01 23:40", at, "full_house.png")],
    )
    forward.reply("05-01 23:41", WORF, hand, "I demand a rematch.")
    forward.say(
        "05-02 18:00",
        TROI,
        f"Nobody tell {mention(PICARD)}!",
        attachments=[attach("05-02 18:00", at, "SPOILER_captains_cake.png")],
        reactions=[reacted("🎂", 8)],
    )
    wave = "AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRob"
    spoken = attach("05-02 18:05", at, "voice-message.ogg", duration_secs=4.0, waveform=wave)  # fmt: skip
    forward.say("05-02 18:05", RIKER, flags=VOICE, attachments=[spoken])
    doubt = "Was that a message or a trombone?"
    forward.say("05-02 18:06", CRUSHER, doubt, sticker_items=[sticker("Facepalm", 2)])
    scale = attach("05-02 18:07", at, "trombone_practice.mp3")
    forward.say("05-02 18:07", RIKER, "*This* is a trombone.", attachments=[scale])
    dance = "https://example.com/gifs/dancing-valet"
    forward.say(
        "05-02 18:10",
        LWAXANA,
        dance,
        embeds=[
            embed(
                "gifv",
                url=dance,
                provider={"name": "Period Gifs"},
                video=picture("https://example.com/gifs/v.mp4"),
            )
        ],
    )
    homn = "https://example.com/mr-homn.png"
    forward.say(
        "05-02 18:12",
        LWAXANA,
        homn,
        embeds=[embed("image", url=homn, thumbnail=picture(homn))],
    )
    tongues = "日本語 · ελληνικά · עברית · tlhIngan Hol 🖖🏽👩‍🚀"
    forward.say("05-02 18:15", GUINAN, tongues)
    forward.say("05-02 18:20", DELETED, "I was never here.")
    forward.say("05-02 18:30", WORF, "…", edited="05-02 18:31")
    ode = forward.say(
        "05-03 08:00",
        DATA,
        "# Ode to Spot\nFelis catus is your taxonomic nomenclature,\n"
        "an endothermic quadruped, carnivorous by nature.",
        sticker_items=[sticker("Cat", 3)],
    )
    forward.reply("05-03 08:05", LAFORGE, ode, "It's… long, Data.")
    meow = "Translated: *meow.*"
    forward.reply("05-03 08:06", COMPUTER, ode, meow, type=23, application_id=COMPUTER)
    date = next(m for m in bridge.said if "<t:" in m["content"])
    forward.forward("05-03 08:10", WESLEY, bridge, date)


def on_stage(theater: Room) -> None:
    theater.say("05-03 19:00", DATA, "An Evening of Spot", type=27)
    theater.say("05-03 19:01", WESLEY, type=29)
    theater.say("05-03 19:10", DATA, "An Evening of Spot, abridged", type=31)
    theater.say("05-03 19:12", RIKER, "Bravo. *Is it over?*")
    theater.say("05-03 19:30", DATA, "An Evening of Spot, abridged", type=28)


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
    blocked = [
        field("rule_name", "No commerce"),
        field("channel_id", sim.id),
        field("decision_id", "47988"),
        field("keyword", "latinum"),
        field("keyword_matched_content", "latinum"),
    ]
    sale = "buy gold-pressed latinum at ferengi.example"
    kind = "auto_moderation_message"
    sim.say(
        "04-10 09:07",
        Q,
        type=24,
        embeds=[embed(kind, description=sale, fields=blocked)],
    )
    sim.say("04-10 09:08", MODERATOR, type=36)
    sim.say("04-10 09:09", MODERATOR, type=37)
    sim.say("04-10 09:10", WESLEY, type=25)
    sim.say("04-10 09:11", WESLEY, type=32)
    sim.say("04-10 09:12", WESLEY, type=44)
    grounds.say("04-11 07:00", BOOTHBY, "The roses do not care what century it is.")
    elm = "I carved initials into an elm here, once. Or will."
    grounds.say("04-11 07:30", PICARD, elm, reactions=[reacted("🌳", 1)])


def direct() -> None:
    riker = dm("04-28 07:00", 1, RIKER)
    riker.say("05-01 08:00", RIKER, "Ready when you are, sir.")
    bridge = "Number One, you have the bridge."
    riker.say("05-01 08:01", PICARD, bridge, reactions=[reacted("🖖", 1)])
    ended = iso(snowflake("05-01 08:20"))
    call = {"participants": [RIKER, PICARD], "ended_timestamp": ended}
    riker.say("05-01 08:02", RIKER, type=3, call=call)
    view = attach("05-01 08:30", riker.id, "earth_from_orbit.jpg")
    riker.say("05-01 08:30", PICARD, "The chair suits you.", attachments=[view])

    poker = dm("04-29 20:00", 3, RIKER, DATA, WORF, name="Poker Night")
    poker.say("04-29 20:00", RIKER, type=1, naming=[WORF])
    poker.say("04-29 20:01", RIKER, "Poker Night", type=4)
    poker.say("04-29 20:02", DATA, type=5)
    poker.say("04-29 20:05", LAFORGE, "Deal me out, shift until 0300.")
    poker.say("04-29 20:06", LAFORGE, type=2, naming=[LAFORGE])
    poker.say("04-29 20:07", RIKER, type=2, naming=[Q])
    poker.say("04-29 20:10", WORF, "Deuces are for children. And *no* Q.")
    stakes = [("Chips", 0, False), ("Shifts", 3, False)]
    open_poll = poll("Stakes?", stakes, ended=False, expiry="04-30 20:30")
    poker.say("04-29 20:30", RIKER, poll=open_poll)

    surgery = dm("04-30 09:00", 3, CRUSHER, TROI)
    surgery.say("04-30 09:00", CRUSHER, "Captain, your physical is overdue.")
    surgery.say("04-30 09:01", TROI, "*Very* overdue.")
    surgery.say("04-30 09:30", PICARD, "I find that I am needed on the bridge.")

    guinan = dm("04-30 10:00", 1, GUINAN)
    guinan.say("05-02 23:00", GUINAN, "Trust the rift, Picard. It's been here before.")
    guinan.say("05-02 23:05", PICARD, "Have you?")

    ghost = dm("04-30 11:00", 1, DELETED)
    ghost.say("04-30 11:00", DELETED, "Do not go back through it.")

    bot = dm("04-30 12:00", 1, COMPUTER)
    usage = embed(title="Help", description="Say `locate`, then a name.")
    inquiry = "Please state the nature of your inquiry."
    bot.say("04-30 12:00", COMPUTER, inquiry, embeds=[usage])

    # Opened, and nothing said in it.
    dm("04-30 13:00", 1, LWAXANA)


# ── the file ─────────────────────────────────────────────────────────


def line(v: Any) -> str:
    return json.dumps(v, ensure_ascii=False)


def rows(v: list[Obj], indent: str) -> str:
    body = ",\n".join(f"{indent} {line(record)}" for record in v)
    return f"[\n{body}\n{indent}]" if v else "[]"


def dump(world: Obj) -> str:
    """One record to a line, so that a change to one message reads as one."""
    parts = []
    for key, value in world.items():
        if isinstance(value, list):
            body = rows(value, " ")
        elif key == "me":
            body = line(value)
        else:
            body = ",\n".join(
                f"  {line(k)}: {rows(v, '  ') if isinstance(v, list) else line(v)}"
                for k, v in value.items()
            )
            body = "{\n" + body + "\n }"
        parts.append(f" {line(key)}: {body}")
    return "{\n" + ",\n".join(parts) + "\n}\n"


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
            ENTERPRISE: [
                role(ENTERPRISE, ENTERPRISE, "@everyone", READ),
                role(SENIOR, ENTERPRISE, "Senior Staff"),
                role(AWAY_TEAM, ENTERPRISE, "Away Team"),
                role(INTERNAL, ENTERPRISE, "Internal Affairs"),
            ],
            ACADEMY: [role(ACADEMY, ACADEMY, "@everyone", READ)],
        },
        "members": {
            ENTERPRISE: membership(ENTERPRISE, [SENIOR, AWAY_TEAM], "Captain"),
            ACADEMY: membership(ACADEMY, [], None),
        },
        "threads": threads,
        "messages": messages,
        "files": links,
    }
    (OUT / "world.json").write_text(dump(world), encoding="utf-8")
    print(f"{sum(map(len, messages.values()))} messages, {len(links)} files")


if __name__ == "__main__":
    main()
