//! Beverly Crusher's Baby Connect, as playback tapes: Wesley's first
//! days, logged by Beverly and Jack. The shapes follow what
//! app.babyconnect.com's web page was sent in November 2026; the values
//! are made up.
//!
//! Baby Connect writes a year as two digits counted from 2000, so
//! Wesley's 2348 is written here as 2048.
//!
//! One child, so every day is asked for him by his id, as the web page
//! asks.
//! [`Edition::Before`] is a run on the evening of 2048-10-16: the home
//! page and the five days from Wesley's birthday. [`Edition::After`] is
//! two days later: the two days the first run left unsettled, one entry
//! on them edited, and the two days since.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_etl_baby_connect::ingest::{api, days};
use datalib_etl_web::http::HttpResponse;
use datalib_etl_web::synthesize::{json_response, write_fixture};
use datalib_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

pub const TOKEN: &str = "dGhlIGVudGVycHJpc2U=";
pub const WESLEY: u64 = 5629499534213121;
pub const BEVERLY: u64 = 5066549580791808;
pub const JACK: u64 = 5066549580791809;

/// The entry Beverly logged on the 15th and Jack corrected on the 17th.
pub const EDITED: u64 = 4785074604081215;
/// An entry of the 13th that the 12th's answer carries too.
pub const SPILLED: u64 = 4785074604081139;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edition {
    Before,
    After,
    /// The nursery's day, [`write_nursery`]'s.
    Nursery,
}

impl Edition {
    pub fn now(self) -> IsoOffsetTimestamp {
        let s = match self {
            Edition::Before => "2048-10-16T20:00:00-07:00",
            Edition::After => "2048-10-18T20:00:00-07:00",
            Edition::Nursery => "2066-03-05T20:00:00-08:00",
        };
        datalib_time::parse_strict(s).unwrap()
    }

    fn days(self) -> &'static [&'static str] {
        match self {
            Edition::Before => &[
                "2048-10-12",
                "2048-10-13",
                "2048-10-14",
                "2048-10-15",
                "2048-10-16",
            ],
            Edition::After => &["2048-10-15", "2048-10-16", "2048-10-17", "2048-10-18"],
            Edition::Nursery => &[],
        }
    }
}

pub fn home_page() -> String {
    let uinfo = json!({
        "Id": BEVERLY, "Name": "Beverly Crusher",
        "myKids": [
            {"Id": WESLEY, "Name": "Wesley", "BDay": "10/12/2048", "Boy": true, "gender": 1,
             "Parents": [BEVERLY, JACK], "Photo": 0, "PhotoInd": 0, "Created": "10/12/2048",
             "url_t": "/seacloud-photos/k1?Expires=1&Signature=a"}
        ]
    });
    format!(
        "<html><head><script>\n__srf_token__ = '{TOKEN}';\nvar _uinfo = {uinfo};\n</script>\
         </head><body>Baby Connect</body></html>"
    )
}

fn html(body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: BTreeMap::from([("content-type".to_string(), "text/html".to_string())]),
        body: body.as_bytes().to_vec(),
        duration_ms: 0,
    }
}

fn entry(id: u64, kid: u64, pdt: u32, utm: u32, cat: u32, by: u64, txt: &str) -> Value {
    json!({
        "Id": id, "Kid": kid, "Pdt": pdt, "Utm": utm, "Cat": cat, "By": by, "Txt": txt,
        "comments": [], "readby": [by], "isst": false, "lId": 0, "meet": false,
        "delayed": false, "pinned": false, "e": "", "ms": 0
    })
}

fn pdt(day: &str) -> u32 {
    days::pdt(days::day(day).unwrap())
}

/// The entries the day's answer carries, its own and any it spills.
fn entries(edition: Edition, day: &str) -> Vec<Value> {
    let p = pdt(day);
    let base = 4785074604081000 + (p % 100) as u64 * 10;
    let mut out = vec![entry(
        base + 1,
        WESLEY,
        p,
        210,
        350,
        BEVERLY,
        "Wesley nursed (left) for 15 min",
    )];
    match day {
        "2048-10-12" => {
            out.push(entry(
                base + 2,
                WESLEY,
                p,
                1430,
                1300,
                JACK,
                "Wesley was born",
            ));
            // The 13th's diaper, which the 12th's answer carries too.
            out.push(entry(
                SPILLED,
                WESLEY,
                pdt("2048-10-13"),
                30,
                402,
                JACK,
                "Wesley had a BM diaper",
            ));
        }
        "2048-10-13" => {
            out.push(entry(
                SPILLED,
                WESLEY,
                p,
                30,
                402,
                JACK,
                "Wesley had a BM diaper",
            ));
            let mut note = entry(base + 3, WESLEY, p, 905, 2600, BEVERLY, "Diary");
            note["n"] = json!("He grabbed Jack's finger and would not let go. Jack says he has an engineer's grip.");
            out.push(note);
        }
        "2048-10-14" => {
            out.push(entry(
                base + 2,
                WESLEY,
                p,
                1700,
                1700,
                JACK,
                "Wesley weighed 7 lbs 2 oz",
            ));
        }
        "2048-10-15" => {
            let mut e = entry(EDITED, WESLEY, p, 2330, 502, BEVERLY, "Wesley slept 2 hrs");
            e["d"] = json!(120);
            if edition == Edition::After {
                e["Txt"] = json!("Wesley slept 3 hrs");
                e["d"] = json!(180);
                e["n"] = json!("Fixed: it was three hours, not two.");
                e["readby"] = json!([BEVERLY, JACK]);
            }
            out.push(e);
        }
        _ => {}
    }
    out
}

fn day_answer(edition: Edition, day: &str) -> Value {
    json!({
        "all": true, "pdt": pdt(day), "deleted": [], "ms": 0, "staff": [], "ts": 0,
        "list": entries(edition, day),
        "users": {
            BEVERLY.to_string(): {"Name": "Beverly Crusher", "Photo": 0, "PhotoInd": 0},
            JACK.to_string(): {"Name": "Jack Crusher", "Photo": 0, "PhotoInd": 0}
        }
    })
}

/// Writes `edition`'s tape under `root`.
pub fn write(root: &Path, edition: Edition) {
    write_fixture(root, &api::home_request(), &html(&home_page())).unwrap();
    for day in edition.days() {
        let req = api::list_request(&WESLEY.to_string(), pdt(day), TOKEN);
        write_fixture(root, &req, &json_response(&day_answer(edition, day))).unwrap();
    }
}

pub const ALEXANDER: u64 = 5629499534213122;
pub const NURSERY: u64 = 5066549580791810;
/// The day the nursery tape holds.
pub const NURSERY_DAY: &str = "2066-03-02";

/// The Enterprise's nursery: an account two families' children are
/// logged under, Wesley and Alexander, so a day is asked for both at
/// once (`Kid=0`). One day, the one a test's window names.
pub fn write_nursery(root: &Path) {
    let uinfo = json!({
        "Id": NURSERY, "Name": "Enterprise Nursery",
        "myKids": [
            {"Id": WESLEY, "Name": "Wesley", "BDay": "10/12/2048", "Boy": true,
             "Parents": [BEVERLY, JACK], "Photo": 0, "PhotoInd": 0},
            {"Id": ALEXANDER, "Name": "Alexander", "BDay": "3/1/2066", "Boy": true,
             "Parents": [], "Photo": 0, "PhotoInd": 0}
        ]
    });
    let page = format!(
        "<html><head><script>\n__srf_token__ = '{TOKEN}';\nvar _uinfo = {uinfo};\n</script>\
         </head><body>Baby Connect</body></html>"
    );
    write_fixture(root, &api::home_request(), &html(&page)).unwrap();
    let p = pdt(NURSERY_DAY);
    let mut bite = entry(
        4785074604090001,
        ALEXANDER,
        p,
        1015,
        2900,
        NURSERY,
        "Incident",
    );
    bite["n"] = json!("Alexander bit the replicator. The replicator is fine.");
    let answer = json!({
        "all": true, "pdt": p, "deleted": [], "ms": 0, "staff": [], "ts": 0,
        "list": [
            entry(4785074604090002, ALEXANDER, p, 930, 350, NURSERY, "Alexander drank 4 oz"),
            bite,
        ],
        "users": [{"Id": NURSERY, "Name": "Enterprise Nursery"}]
    });
    let req = api::list_request("0", p, TOKEN);
    write_fixture(root, &req, &json_response(&answer)).unwrap();
}

/// A signed-out session: the home page is the login page.
pub fn write_signed_out(root: &Path) {
    let login = r#"<form id="loginForm"><input type="hidden" id="__srf_token__"
        name="__srf_token__"></form>"#;
    write_fixture(root, &api::home_request(), &html(login)).unwrap();
}
