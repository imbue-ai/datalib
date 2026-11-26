"""What made the TNG corpus's checked-in `datalib_curated/`, through the
applets' routes on a root `materialize_tng_root` built
(datalib/backend/curated_overlay/README.md).
The stores are the fixture; this is the record of how they were made, and
the way to make them again from nothing.

    python3 tests/fixtures/curated/curate_tng.py <state_dir>
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from curated_client import Applets, png

a = Applets(sys.argv[1])

picard = a.make(
    "Jean-Luc Picard",
    [
        "email:picard@enterprise.starfleet",
        "email:jlpicard@enterprise.starfleet",
        "email:jlpicard@starfleet.gov",
        "email:jean-luc.picard@enterprise.starfleet.invalid",
        "tel:+12025550101",
        "slack:T_NCC1701D/U_PICARD",
        "slack:T1701D/U0PICARD",
        "discord:170100000000000001",
        "linkedin:jlpicard",
        "github:jlpicard",
        "gitlab:gitlab.com/jlpicard",
    ],
)
a.contacts(
    "POST", "/stopped_working", {"handle": "email:jlpicard@starfleet.gov", "by": "2364"}
)
a.contacts("POST", "/me", {"contact_id": picard, "me": True})

riker = a.make(
    "William T. Riker",
    [
        "email:riker@enterprise.starfleet",
        "email:riker@enterprise.org",
        "email:wriker@enterprise.starfleet",
        "email:will.riker@alaska.earth",
        "tel:+17015550101",
        "slack:T_NCC1701D/U_RIKER",
        "discord:170100000000000002",
        "linkedin:wriker",
        "github:wtriker",
        "gitlab:gitlab.com/wtriker",
    ],
)
# Signal knows him as "Will Riker": a contact of its own, then merged.
will = a.make("Will Riker", ["signal_aci:0195683a-d140-87f9-bdf6-234da6d6880c"])
a.contacts("POST", "/merge", {"from": will, "into": riker})
a.contacts("PUT", f"/photo/{riker}", raw=png(), ctype="image/png")
a.edit_fields(
    riker,
    [
        {
            "field_id": "f-riker-title",
            "kind": "title",
            "label": None,
            "value": "First Officer",
            "copied_from": {"source_id": "tng_contacts", "key": "Bridge#tng-riker"},
        },
        {
            "field_id": "f-riker-plays",
            "kind": "other",
            "label": "plays",
            "value": "Trombone",
            "copied_from": None,
        },
    ],
    note="Made up, for the TNG fixture's curated overlay.",
)

data = a.make(
    "Data",
    [
        "email:data@enterprise.starfleet",
        "tel:+17015550102",
        "slack:T_NCC1701D/U_DATA",
        "discord:170100000000000003",
        "linkedin:datasoong",
        "github:data-soong",
        "signal_aci:0195683a-d140-87f9-bdf6-234da6d68811",
    ],
)
troi = a.make(
    "Deanna Troi",
    [
        "email:troi@enterprise.starfleet",
        "tel:+17015550105",
        "tel:+12025550103",
        "discord:170100000000000004",
        "signal_aci:0195683a-d140-87f9-bdf6-234da6d68814",
    ],
)
worf = a.make(
    "Worf",
    [
        "email:worf@enterprise.starfleet",
        "tel:+12025550102",
        "tel:+17015550104",
        "slack:T_NCC1701D/U_WORF",
        "slack:T1701D/U0WORF",
        "discord:170100000000000005",
        "signal_aci:0195683a-d140-87f9-bdf6-234da6d68813",
    ],
)
geordi = a.make(
    "Geordi La Forge",
    [
        "email:glaforge@enterprise.starfleet",
        "email:laforge@enterprise.starfleet",
        "email:geordi.laforge@enterprise.starfleet.invalid",
        "tel:+12025550104",
        "tel:+17015550103",
        "discord:170100000000000006",
        "signal_aci:0195683a-d140-87f9-bdf6-234da6d68812",
    ],
)
beverly = a.make(
    "Beverly Crusher",
    [
        "email:beverly.crusher@enterprise.starfleet.invalid",
        "tel:+17015550106",
        "discord:170100000000000007",
        "tel:+12025550142",
    ],
)
# Wesley's number, linked to his mother by mistake and taken off.
a.contacts("POST", "/unlink", {"handle": "tel:+12025550142"})
# A shared address belongs to a group contact: the ship's status mail.
ship = a.make("Enterprise systems", ["email:lcars@enterprise.starfleet"], kind="group")

# An agent proposes; Picard confirms some, turns one down, leaves one.
print(
    a.contacts(
        "POST",
        "/proposals",
        {
            "agent": "tng-curator",
            "run": "tng-seed",
            "contacts": [
                {
                    "contact_id": worf,
                    "handles": ["gitlab:gitlab.com/worf-rozhenko"],
                    "reason": "GitLab's name for the account is Worf",
                    "confidence": 0.8,
                    "labels": [],
                },
                {
                    "contact_id": data,
                    "handles": ["tel:+15551234001"],
                    "reason": "Beeper shows this number as Mr. Data",
                    "confidence": 0.6,
                    "labels": [],
                },
                {
                    "contact_id": worf,
                    "handles": ["tel:+12025550153"],
                    "reason": "a security chief's number",
                    "confidence": 0.3,
                    "labels": [],
                },
            ],
            "handles": [
                {
                    "handle": "email:lcars@enterprise.starfleet",
                    "labels": [
                        {
                            "label": "real",
                            "applies": True,
                            "reason": "the ship's own status mail",
                            "confidence": 0.9,
                        }
                    ],
                },
                {
                    "handle": "email:prizes@quarks.example",
                    "labels": [
                        {
                            "label": "real",
                            "applies": False,
                            "reason": "a prize draw nobody entered",
                            "confidence": 0.9,
                        }
                    ],
                },
            ],
        },
    )
)
a.contacts(
    "POST",
    "/review/confirm",
    {
        "subjects": [
            "gitlab:gitlab.com/worf-rozhenko",
            "email:lcars@enterprise.starfleet",
            "email:prizes@quarks.example",
        ]
    },
)
a.contacts("POST", "/review/reject", {"subjects": ["tel:+12025550153"]})

senior = [f"contact:{c}" for c in (picard, riker, data, troi, worf, geordi, beverly)]
a.contacts(
    "POST", "/label", {"subjects": senior, "label": "Senior staff", "applies": True}
)
a.contacts(
    "POST",
    "/label",
    {"subjects": ["tel:+12025550142"], "label": "Senior staff", "applies": False},
)

# Minor problems, muted: two by Picard, one by an agent, and an info row.
print(
    a.mute(
        [
            "6df47df9-b6ad-5372-942e-5db5e4d068bb",  # claude-api: a stardate is no date
            "1860e56d-3cbf-504d-ab0b-df061b1bfe4a",  # slack: a call block, not shown
        ]
    )
)
print(
    a.mute(["4a7fcf9d-734b-5848-8e07-f0cd6bcd7491"], agent="tng-curator")
)  # whatsapp: a missing photo
