use super::*;

fn item(author: &str, created_at: &str, text: &str) -> Piece {
    Piece {
        uuid: Some(format!("m-{author}")),
        text: text.into(),
        author: Some(author.into()).filter(|a: &String| !a.is_empty()),
        created_at: Some(created_at.into()).filter(|t: &String| !t.is_empty()),
        ..Piece::default()
    }
}

fn addressed(people: &[(&str, &str, Option<&str>)]) -> Vec<Addressed> {
    people
        .iter()
        .map(|(role, name, handle)| Addressed {
            role: role.to_string(),
            name: name.to_string(),
            handle: handle.map(String::from),
        })
        .collect()
}

/// Every kind of piece the drawing tells apart, in made-up TNG data.
fn golden() -> Vec<Piece> {
    let mut email = item(
        "Will Riker",
        "2026-03-03T14:05:00-08:00",
        "Captain, the away team is ready.",
    );
    email.author_handle = Some("email:riker@enterprise.starfleet".into());
    email.addressed = addressed(&[
        ("cc", "Deanna Troi", None),
        (
            "to",
            "Jean-Luc Picard",
            Some("email:picard@enterprise.starfleet"),
        ),
        ("bcc", "Worf", None),
        ("to", "Data", Some("tel:+17015550101")),
        ("to", "", Some("email:laforge@enterprise.starfleet")),
    ]);
    email.labels = vec!["Away missions".into(), " ".into(), "Inbox".into()];
    let mut slack = item("Data", "2026-03-03T22:05:00Z", "Acknowledged.");
    slack.author_handle = Some("slack:T_NCC1701D/U_DATA".into());
    vec![
        email,
        slack,
        item("", "2026-03-04T09:00:00+01:00", "Ensign Crusher joined."),
        item("", "", "A note with no author and no time."),
        Piece {
            uuid: None,
            text: "<div>Page 2: the warp core schematic</div>\n".into(),
            framed: true,
            ..Piece::default()
        },
        item("Geordi La Forge", "2026-03-05T08:00:00-05:00", "   "),
        item("Wesley Crusher", "stardate 47457.1", "Sir?"),
    ]
}

fn drawn_golden() -> String {
    draw(&golden())
        .iter()
        .map(|s| format!("[{:?} lead={:?}]\n{}", s.uuid, s.lead, s.text))
        .collect()
}

/// Each version and the blake3 of what it draws for [`golden`], oldest
/// first. A change to the drawing appends a line and raises
/// [`INDEX_TEXT_VERSION`]; it never edits a line already here.
const DRAWN: &[(u32, &str)] = &[
    (
        1,
        "2665328eae4a1fe16371f2e1480dfe4dd4463943a6e6a85952ae5c87660bef11",
    ),
    (
        2,
        "2d3c1fa43b9a762628b7c400b4d812b4d67678acde7305d21c5b098b17c740fa",
    ),
];

/// The guard on [`INDEX_TEXT_VERSION`]: a drawing that changes while the
/// version stays would leave every document drawn the old way, and its
/// chunks embedded from text the index no longer draws.
#[test]
fn the_golden_draw_is_this_versions() {
    let out = drawn_golden();
    let hash = blake3::hash(out.as_bytes()).to_hex().to_string();
    let (last, pinned) = DRAWN.last().expect("a version");
    assert!(
        DRAWN.windows(2).all(|w| w[0].0 < w[1].0),
        "versions only go up"
    );
    assert_eq!(
        *last, INDEX_TEXT_VERSION,
        "the newest line is this version's"
    );
    assert_eq!(
        hash,
        *pinned,
        "the index text changed. Raise INDEX_TEXT_VERSION and add a line \
         ({}, \"{hash}\") to DRAWN; do not edit the old one. It now draws:\n{out}",
        INDEX_TEXT_VERSION + 1
    );
}

/// An item is its envelope, its labels, then its body. A person is named
/// with the address one would type for them, an email or a phone number,
/// so a pasted address finds the item by its words; a handle nobody types
/// (a Slack id) is left out.
#[test]
fn an_item_is_its_envelope_its_labels_then_its_body() {
    let drawn = draw(&golden());
    let lead = "Will Riker <riker@enterprise.starfleet> to Jean-Luc Picard \
                <picard@enterprise.starfleet>, Data <+17015550101>, \
                laforge@enterprise.starfleet; cc Deanna Troi; bcc Worf — Tue 3 Mar 2026 14:05";
    assert_eq!(drawn[0].lead.as_deref(), Some(lead));
    assert_eq!(
        drawn[0].text,
        format!("{lead}\n\nLabels: Away missions, Inbox\n\nCaptain, the away team is ready.\n\n")
    );
    assert_eq!(
        drawn[1].lead.as_deref(),
        Some("Data — Tue 3 Mar 2026 22:05")
    );
}

/// A date is drawn on the clock its source wrote, never the machine's:
/// one instant written at two offsets draws two times, and nothing here
/// reads a clock or a zone.
#[test]
fn a_date_keeps_the_offset_it_was_written_in() {
    assert_eq!(when("2026-03-03T14:05:00-08:00"), "Tue 3 Mar 2026 14:05");
    assert_eq!(when("2026-03-03T23:05:00+01:00"), "Tue 3 Mar 2026 23:05");
    assert_eq!(when("2026-03-03T22:05:00Z"), "Tue 3 Mar 2026 22:05");
    assert_eq!(when("stardate 47457.1"), "stardate 47457.1");
}

#[test]
fn a_framed_piece_is_drawn_as_it_is_and_an_empty_item_as_its_envelope() {
    let drawn = draw(&golden());
    assert_eq!(
        drawn[4].text,
        "<div>Page 2: the warp core schematic</div>\n"
    );
    assert_eq!(drawn[4].lead, None);
    assert_eq!(drawn[5].text, "Geordi La Forge — Thu 5 Mar 2026 08:00\n\n");
    assert_eq!(drawn[3].lead, None);
    assert_eq!(drawn[3].text, "A note with no author and no time.\n\n");
    assert_eq!(draw(&[item("", "", "")])[0].text, "");
}
