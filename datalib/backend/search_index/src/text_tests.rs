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
    let mut slack = item(
        "Data",
        "2026-03-03T22:05:00Z",
        "[@Jean-Luc Picard](slack://user?team=T_NCC1701D&id=U_PICARD \"@Jean-Luc Picard \
         (slack:T_NCC1701D/U_PICARD)\"), acknowledged.",
    );
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
        sender_named_with_address(),
        item(
            "Worf",
            "2026-03-06T08:00:00-08:00",
            "<details><summary>Tool use: phaser_check</summary>\n\n&lt;armed&gt; \\| ready\n\n\
             thread '<unnamed>' panicked; a Vec<String> of <T>\n\n</details>",
        ),
    ]
}

/// An email's sender as the email render names them: as `From:` reads,
/// address and all.
fn sender_named_with_address() -> Piece {
    let mut p = item(
        "Beverly Crusher <Crusher@Enterprise.starfleet>",
        "2026-03-06T07:30:00-08:00",
        "Sickbay is ready.",
    );
    p.author_handle = Some("email:crusher@enterprise.starfleet".into());
    p
}

/// A sender whose name already carries their address was drawn with it
/// twice, `Beverly Crusher <crusher@…> <crusher@…>`, in every chunk of
/// their mail.
#[test]
fn a_name_that_carries_its_address_names_it_once() {
    let drawn = draw(&[sender_named_with_address()], &Facts::default());
    assert_eq!(
        drawn[0].lead.as_deref(),
        Some("Beverly Crusher <Crusher@Enterprise.starfleet> — Fri 6 Mar 2026 07:30")
    );
}

/// A page as a renderer keeps it: one piece whole, front matter, tags,
/// links and escapes and all, and the facts its front matter holds.
fn golden_page() -> (Vec<Piece>, Facts) {
    let text = "---\nmarkdown_uuid: 00000000-0000-8000-8000-000000000001\nprovider: calendar\n\
                kind: Recurring Event\ncalendar: \"Bridge\"\ntitle: \"Senior staff\"\n\
                external_id: \"c_1701@group.calendar.test#a1\"\n---\n\n\
                <h1 class=\"page-title\" data-page-title-uuid=\"p\">Senior staff \
                <a class=\"source-link\" href=\"https://calendar.test/e\" target=\"_blank\">↗</a></h1>\n\n\
                | | |\n| --- | --- |\n\
                | When | <span class=\"time-range\"><time datetime=\"2026-03-03T09:00:00-08:00\" \
                title=\"2026-03-03 09:00:00 -08:00\">Tue Mar 3rd, 2026 at 09:00</time></span> |\n\
                | Organizer | [Will Riker](mailto:riker@enterprise.test \"Will Riker <riker@enterprise.test>\") |\n\n\
                <dl class=\"contact-fields\">\n<dt>Room</dt><dd>Observation&nbsp;lounge</dd>\n</dl>\n\n\
                Saucer \\| drive section &amp; [the log](datalib:doc/x \"Log\")<br>Q&A &bogus;\n\n\
                `<b>kept</b> &amp;` and <https://enterprise.test> and <picard@enterprise.test>\n";
    let page = vec![
        Piece {
            uuid: None,
            text: text.into(),
            framed: true,
            ..Piece::default()
        },
        Piece {
            uuid: Some("m-2".into()),
            text: "---\nnot: front matter\n---\n<div id=\"m-2\">Page 2 &lt;draft&gt;</div>\n"
                .into(),
            framed: true,
            ..Piece::default()
        },
    ];
    let facts = Facts {
        source_name: "Calendar".into(),
        front: [
            ("markdown_uuid", "00000000-0000-8000-8000-000000000001"),
            ("provider", "calendar"),
            ("kind", "Recurring Event"),
            ("calendar", "Bridge"),
            ("title", "Senior staff"),
            ("external_id", "c_1701@group.calendar.test#a1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    };
    (page, facts)
}

fn drawn_golden() -> String {
    let (page, facts) = golden_page();
    draw(&golden(), &Facts::default())
        .iter()
        .chain(draw(&page, &facts).iter())
        .map(|s| format!("[{:?} lead={:?}]\n{}", s.uuid, s.lead, s.text))
        .collect()
}

/// A page was indexed as its rendered markdown: YAML front matter, tags
/// and their ids, link targets and escapes, half of each chunk on some
/// pages, and words like `class` and `data` matched every page. It is
/// drawn as the words a person reads, after a line of the facts worth
/// searching for.
#[test]
fn a_page_opens_with_its_facts_and_reads_as_its_words() {
    let (page, facts) = golden_page();
    let drawn = draw(&page, &facts);
    assert_eq!(
        drawn[0].text,
        "Source: Calendar\nKind: Recurring Event\nCalendar: Bridge\n\n\
         Senior staff\n\n\
         | When | Tue Mar 3rd, 2026 at 09:00 |\n\
         | Organizer | Will Riker <riker@enterprise.test> |\n\n\
         Room: Observation\u{a0}lounge\n\n\
         Saucer | drive section & the log\nQ&A &bogus;\n\n\
         `<b>kept</b> &amp;` and <https://enterprise.test> and <picard@enterprise.test>\n"
    );
    assert_eq!(drawn[0].lead, None);
    assert_eq!(
        drawn[1].text, "---\nnot: front matter\n---\nPage 2 <draft>\n",
        "only the page's opening piece holds its front matter"
    );
}

/// A page whose first piece is its front matter alone: the facts line
/// stood as a chunk of its own, apart from every word it describes.
#[test]
fn the_facts_open_the_first_piece_with_words() {
    let (_, facts) = golden_page();
    let page = [
        Piece {
            text: "---\nprovider: calendar\n---\n\n".into(),
            framed: true,
            ..Piece::default()
        },
        Piece {
            text: "# Senior staff\n".into(),
            framed: true,
            ..Piece::default()
        },
    ];
    let drawn = draw(&page, &facts);
    assert_eq!(drawn[0].text, "");
    assert!(
        drawn[1].text.starts_with("Source: Calendar\n"),
        "{}",
        drawn[1].text
    );
}

/// A message's body carried its renderer's markup to the index: every
/// thinking block and tool call wrapped in `<details><summary>`, a voice
/// note's `<audio>`, a `<time>`, an SMS's `&lt;#&gt;`. It is drawn as its
/// words, as a page is; a conversation has no facts line.
#[test]
fn a_message_is_drawn_as_its_words_and_has_no_facts_line() {
    let (_, facts) = golden_page();
    let body = item(
        "Data",
        "2026-03-03T22:05:00Z",
        "<details><summary>Tool use: tricorder_scan</summary>\n\n\
         &lt;#&gt; code \\| <b>x</b> at <time datetime=\"2369-01-01\">1 Jan</time>\n\n\
         <audio controls src=\"/data/x/blob/1\"></audio>\n\n`<b>kept</b>`\n\n</details>",
    );
    let drawn = draw(&[body], &facts);
    assert_eq!(
        drawn[0].text,
        "Data — Tue 3 Mar 2026 22:05\n\nTool use: tricorder_scan\n\n\
         <#> code | x at 1 Jan\n\n`<b>kept</b>`\n\n"
    );
}

/// A page's head is its facts line, which every chunk repeats; a
/// conversation has none, since its chunks repeat their item's envelope.
#[test]
fn a_page_has_a_head_and_a_conversation_does_not() {
    let (page, facts) = golden_page();
    assert_eq!(
        head(&page, &facts).as_deref(),
        Some("Source: Calendar\nKind: Recurring Event\nCalendar: Bridge")
    );
    assert!(draw(&page, &facts)[0]
        .text
        .starts_with(&head(&page, &facts).unwrap()));
    assert_eq!(head(&golden(), &facts), None);
}

/// A `<word>` was taken for a tag whatever the word, so a panic's
/// `thread '<unnamed>'` was indexed as `thread ''`. Only HTML's own
/// elements are tags; any other `<word>` is text.
#[test]
fn only_an_html_element_is_a_tag() {
    for (body, drawn) in [
        ("thread '<unnamed>' panicked", "thread '<unnamed>' panicked"),
        ("a Vec<String> of <T>", "a Vec<String> of <T>"),
        ("<b>bold</b> and <Summary>S</Summary>", "bold and S"),
        ("<custom-tag>x</custom-tag>", "<custom-tag>x</custom-tag>"),
    ] {
        assert_eq!(flatten(body), drawn, "{body}");
    }
}

/// A link whose text spans lines, as a mail's HTML converts to
/// (`[[\n\nFollow](…)](…)`), was left as written, its addresses and all,
/// since links were found one line at a time.
#[test]
fn a_link_whose_text_spans_lines_is_drawn_as_its_text() {
    assert_eq!(
        flatten("[[ \n\n Follow](https://x.test/follow?id=1701)](https://x.test/track?u=2)\nnext"),
        " \n\n Follow\nnext"
    );
    assert_eq!(
        flatten("a [ bracket that\nnever closes\nand more"),
        "a [ bracket that\nnever closes\nand more",
        "an unclosed bracket stays as written"
    );
}

/// The source is named as its renderer labels it, else as the source a
/// page is about (a storage report's), else by the product's name; a fact
/// with no value, and one not worth reading, says nothing. Storage reports
/// all said `Source: datalib`, and names were spelled two ways (`Notion`,
/// `github`).
#[test]
fn the_facts_line_names_the_source_and_the_facts_worth_reading() {
    let facts = |name: &str, front: &[(&str, &str)]| Facts {
        source_name: name.into(),
        front: front
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    assert_eq!(
        facts_line(&facts(
            "GitHub",
            &[
                ("head_sha", "68ed9dc7"),
                ("repo", "ncc-1701/engineering"),
                ("pr_number", "1701"),
                ("state", "merged"),
                ("author", "laforge"),
                ("head_ref", "warp-core"),
                ("base_ref", "main"),
            ]
        )),
        "Source: GitHub\nRepository: ncc-1701/engineering\nPull request: 1701\n\
         State: merged\nAuthor: laforge\nBranch: warp-core\nInto: main"
    );
    assert_eq!(
        facts_line(&facts(
            "Google Drive",
            &[("source_label", "Google Drive"), ("group", " ")]
        )),
        "Source: Google Drive"
    );
    assert_eq!(
        facts_line(&facts(
            "datalib",
            &[("source", "enterprise-mail"), ("measured_at", "x")]
        )),
        "Source: enterprise-mail",
        "a storage report names the source it measures"
    );
    assert_eq!(
        facts_line(&facts(
            "PDF",
            &[("source_path", "logs/2369/warp_core.pdf"), ("blake3", "ab")]
        )),
        "Source: PDF\nPath: logs/2369/warp_core.pdf",
        "a file's path names the folders it sits in"
    );
    assert_eq!(facts_line(&facts("", &[])), "");
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
        "7c415e9eda1ec5a5ff596af6f57a312172c6d5fd5b3c2f6c780ac885d9faeb3c",
    ),
    // The same text, chunked at each item.
    (
        3,
        "7c415e9eda1ec5a5ff596af6f57a312172c6d5fd5b3c2f6c780ac885d9faeb3c",
    ),
    // A name that already carries its address is not given it twice.
    (
        4,
        "4f89acbbbfb514fefb9b523fbe5eb6e55db339e747b6560d6ee0151aa62f1d31",
    ),
    // A page opens with its facts; a page and a message are drawn as their
    // words.
    (
        5,
        "71ddcdd5d3bfc7fc0315a4aec050de3f61f9975240c2d18b20b808c64402e804",
    ),
    // Only HTML's own elements are tags; any other `<word>` is text.
    (
        6,
        "18d549e12746d78b7d2d99071a7de0b3416475eb593930abe37e5fbf66fc4537",
    ),
    // The same text, chunked by the model's tokens.
    (
        7,
        "18d549e12746d78b7d2d99071a7de0b3416475eb593930abe37e5fbf66fc4537",
    ),
    // A page names its source by the product (`GitHub`), a storage report
    // by the source it measures, a file by its path; a link's text may
    // span lines.
    (
        8,
        "9b930274f1097c64ce76c998d4199271038d8fef32fbdfb9d8267a86faa5b7fa",
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
    let drawn = draw(&golden(), &Facts::default());
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
    assert_eq!(
        drawn[1].text,
        "Data — Tue 3 Mar 2026 22:05\n\n@Jean-Luc Picard, acknowledged.\n\n"
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
fn a_framed_piece_is_drawn_as_its_words_and_an_empty_item_as_its_envelope() {
    let drawn = draw(&golden(), &Facts::default());
    assert_eq!(drawn[4].text, "Page 2: the warp core schematic\n");
    assert_eq!(drawn[4].lead, None);
    assert_eq!(drawn[5].text, "Geordi La Forge — Thu 5 Mar 2026 08:00\n\n");
    assert_eq!(drawn[3].lead, None);
    assert_eq!(drawn[3].text, "A note with no author and no time.\n\n");
    assert_eq!(draw(&[item("", "", "")], &Facts::default())[0].text, "");
}

/// A body's links say what a person reads: a mention its name, an email
/// or phone link the address beside it, an image its alt text, a shared
/// link its text. Code keeps its brackets.
#[test]
fn a_bodys_links_are_drawn_as_their_text() {
    for (body, drawn) in [
        (
            "[@Geordi](slack://user?team=T&id=U \"@Geordi (slack:T/U)\") ready?",
            "@Geordi ready?",
        ),
        (
            "ask [Deanna Troi](mailto:troi@enterprise.starfleet \"Deanna Troi\")",
            "ask Deanna Troi <troi@enterprise.starfleet>",
        ),
        ("call [+17015550101](tel:+17015550101)", "call +17015550101"),
        (
            "![a tribble](/data/x/blob/1) and [the log](https://x.test/a)",
            "a tribble and the log",
        ),
        (
            "`arr[i](x)` stays, [Q] is no link",
            "`arr[i](x)` stays, [Q] is no link",
        ),
        ("```\n[a](b)\n```\n[a](b)", "```\n[a](b)\n```\na"),
        (
            "[nested [brackets]](https://x.test \"a ) in a title\")",
            "nested [brackets]",
        ),
    ] {
        assert_eq!(flatten(body), drawn, "{body}");
    }
}
