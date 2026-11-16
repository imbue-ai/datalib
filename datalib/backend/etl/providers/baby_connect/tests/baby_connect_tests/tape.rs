//! Keiko O'Brien's Baby Connect, as playback tapes. The shapes follow
//! what app.babyconnect.com's web page was sent in November 2026; the
//! values are made up.
//!
//! Baby Connect writes a year as two digits counted from 2000, so the
//! crew's 2368 is written here as 2068.
//!
//! Two children, so every day is asked for both at once (`Kid=0`).
//! [`Edition::Before`] is a run on the evening of 2068-10-16: the home
//! page and the five days from Molly's birthday. [`Edition::After`] is
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
pub const MOLLY: u64 = 5629499534213121;
pub const YOSHI: u64 = 5629499534213122;
pub const KEIKO: u64 = 5066549580791808;
pub const MILES: u64 = 5066549580791809;

/// The entry Keiko logged on the 15th and Miles corrected on the 17th.
pub const EDITED: u64 = 4785074604081215;
/// An entry of the 13th that the 12th's answer carries too.
pub const SPILLED: u64 = 4785074604081139;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edition {
    Before,
    After,
}

impl Edition {
    pub fn now(self) -> IsoOffsetTimestamp {
        let s = match self {
            Edition::Before => "2068-10-16T20:00:00-07:00",
            Edition::After => "2068-10-18T20:00:00-07:00",
        };
        datalib_time::parse_strict(s).unwrap()
    }

    fn days(self) -> &'static [&'static str] {
        match self {
            Edition::Before => &[
                "2068-10-12",
                "2068-10-13",
                "2068-10-14",
                "2068-10-15",
                "2068-10-16",
            ],
            Edition::After => &["2068-10-15", "2068-10-16", "2068-10-17", "2068-10-18"],
        }
    }
}

pub fn home_page() -> String {
    let uinfo = json!({
        "Id": KEIKO, "Name": "Keiko O'Brien",
        "myKids": [
            {"Id": MOLLY, "Name": "Molly", "BDay": "10/12/2068", "Boy": false, "gender": 2,
             "Parents": [KEIKO, MILES], "Photo": 0, "PhotoInd": 0, "Created": "10/12/2068",
             "url_t": "/seacloud-photos/k1?Expires=1&Signature=a"},
            {"Id": YOSHI, "Name": "Yoshi", "BDay": "10/14/2068", "Boy": true, "gender": 1,
             "Parents": [KEIKO, MILES], "Photo": 0, "PhotoInd": 0, "Created": "10/14/2068",
             "url_t": "/seacloud-photos/k2?Expires=1&Signature=b"}
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
        MOLLY,
        p,
        210,
        350,
        KEIKO,
        "Molly nursed (left) for 15 min",
    )];
    match day {
        "2068-10-12" => {
            out.push(entry(
                base + 2,
                MOLLY,
                p,
                1430,
                1300,
                MILES,
                "Molly was born",
            ));
            // The 13th's diaper, which the 12th's answer carries too.
            out.push(entry(
                SPILLED,
                MOLLY,
                pdt("2068-10-13"),
                30,
                402,
                MILES,
                "Molly had a BM diaper",
            ));
        }
        "2068-10-13" => {
            out.push(entry(
                SPILLED,
                MOLLY,
                p,
                30,
                402,
                MILES,
                "Molly had a BM diaper",
            ));
            let mut note = entry(base + 3, MOLLY, p, 905, 2600, KEIKO, "Diary");
            note["n"] = json!("She grabbed Miles's finger and would not let go. Chief is smitten.");
            out.push(note);
        }
        "2068-10-14" => {
            out.push(entry(
                base + 2,
                YOSHI,
                p,
                1700,
                1700,
                MILES,
                "Yoshi weighed 7 lbs 2 oz",
            ));
        }
        "2068-10-15" => {
            let mut e = entry(EDITED, YOSHI, p, 2330, 502, KEIKO, "Yoshi slept 2 hrs");
            e["d"] = json!(120);
            if edition == Edition::After {
                e["Txt"] = json!("Yoshi slept 3 hrs");
                e["d"] = json!(180);
                e["n"] = json!("Fixed: it was three hours, not two.");
                e["readby"] = json!([KEIKO, MILES]);
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
            KEIKO.to_string(): {"Name": "Keiko O'Brien", "Photo": 0, "PhotoInd": 0},
            MILES.to_string(): {"Name": "Miles O'Brien", "Photo": 0, "PhotoInd": 0}
        }
    })
}

/// Writes `edition`'s tape under `root`.
pub fn write(root: &Path, edition: Edition) {
    write_fixture(root, &api::home_request(), &html(&home_page())).unwrap();
    for day in edition.days() {
        let req = api::list_request("0", pdt(day), TOKEN);
        write_fixture(root, &req, &json_response(&day_answer(edition, day))).unwrap();
    }
}

/// A signed-out session: the home page is the login page.
pub fn write_signed_out(root: &Path) {
    let login = r#"<form id="loginForm"><input type="hidden" id="__srf_token__"
        name="__srf_token__"></form>"#;
    write_fixture(root, &api::home_request(), &html(login)).unwrap();
}
