//! Minimal MDL `outer-cell` walker, shared by `youtube_watch_history`
//! and `gemini_apps`.

/// Yield each MDL outer-cell as a substring of `html`. The end of one
/// cell is wherever the next cell starts; the final cell runs to
/// EOF (the trailing `</body></html>` chrome is harmless for the
/// per-cell field walkers, which only `find` for known anchors).
pub fn iter_cells(html: &str) -> impl Iterator<Item = &str> {
    let needle = "<div class=\"outer-cell";
    let mut starts: Vec<usize> = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find(needle) {
        let abs = search_from + rel;
        starts.push(abs);
        search_from = abs + needle.len();
    }
    let len = html.len();
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(len))
        .collect();
    starts.into_iter().zip(ends).map(move |(s, e)| &html[s..e])
}

/// Find every `<a href="…">…</a>` inside `cell` and yield
/// `(href, inner_text)`, entities decoded. Inner text is **NOT** stripped
/// of nested tags — for MDL cells the anchor contents are bare strings,
/// so the simple `<a>(.*?)</a>` matches what's there.
pub fn iter_anchors(cell: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cursor = 0;
    while let Some(open_rel) = cell[cursor..].find("<a ") {
        let open = cursor + open_rel;
        // Find the `href="…"` attribute, if present.
        let href_key = "href=\"";
        let Some(href_start_rel) = cell[open..].find(href_key) else {
            break;
        };
        let href_start = open + href_start_rel + href_key.len();
        let Some(href_close_rel) = cell[href_start..].find('"') else {
            break;
        };
        let href_close = href_start + href_close_rel;
        let href = decode_entities(&cell[href_start..href_close]);
        // Body runs from the close of the opening tag to the next `</a>`.
        let Some(open_close_rel) = cell[href_close..].find('>') else {
            break;
        };
        let body_start = href_close + open_close_rel + 1;
        let Some(close_rel) = cell[body_start..].find("</a>") else {
            break;
        };
        let body_end = body_start + close_rel;
        let body = decode_entities(&cell[body_start..body_end]);
        out.push((href, body));
        cursor = body_end + "</a>".len();
    }
    out
}

/// The `src` of every `<img>` in `html`, entities decoded.
pub fn iter_img_srcs(html: &str) -> Vec<String> {
    html.split("<img ")
        .skip(1)
        .filter_map(|tag| {
            let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
            let (_, after) = tag.split_once("src=\"")?;
            let (src, _) = after.split_once('"')?;
            Some(decode_entities(src))
        })
        .collect()
}

/// What is inside the `<div>` whose opening tag ends with `opening_end`
/// (`mdl-typography--body-1">`), nested `<div>`s and all.
pub fn div_contents<'a>(html: &'a str, opening_end: &str) -> Option<&'a str> {
    let start = html.find(opening_end)? + opening_end.len();
    let mut depth = 1;
    let mut at = start;
    loop {
        let open = html[at..].find("<div").map(|i| at + i);
        let close = at + html[at..].find("</div>")?;
        match open {
            Some(open) if open < close => {
                depth += 1;
                at = open + "<div".len();
            }
            _ => {
                depth -= 1;
                if depth == 0 {
                    return Some(&html[start..close]);
                }
                at = close + "</div>".len();
            }
        }
    }
}

/// Decode the character references Google's exporter writes: a handful
/// of named ones (`&amp;`, `&quot;`, `&nbsp;`, …) and any numeric one
/// (`&#39;`). A reference it does not know stays as written.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest
            .get(1..rest.len().min(12))
            .and_then(|window| window.find(';'))
            .and_then(|semi| Some((entity(&rest[1..1 + semi])?, semi + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    let numeric = |digits: &str, radix| char::from_u32(u32::from_str_radix(digits, radix).ok()?);
    match name {
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "nbsp" => Some('\u{a0}'),
        "emsp" => Some('\u{2003}'),
        _ => match name.strip_prefix('#')? {
            hex if hex.starts_with(['x', 'X']) => numeric(&hex[1..], 16),
            dec => numeric(dec, 10),
        },
    }
}

/// Find the last "Month Day, Year, HH:MM:SS AM/PM TZ" chunk inside
/// the cell — Google appends the timestamp at the very end of the
/// entry's body cell, after the anchors / prompt text. Returns the
/// raw timestamp string (not parsed); the caller routes it through
/// [`super::time::parse_mdl_grid`].
pub fn last_timestamp_chunk(cell: &str) -> Option<String> {
    let text = strip_tags(cell);
    let mut ampm_idx: Option<usize> = None;
    for marker in [" AM ", " PM "] {
        if let Some(i) = text.rfind(marker) {
            ampm_idx = Some(ampm_idx.map(|x| x.max(i)).unwrap_or(i));
        }
    }
    let ampm = ampm_idx?;
    let mut start = ampm.saturating_sub(30);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let after = &text[ampm + 4..];
    let tz_end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    let end = ampm + 4 + tz_end;
    let prefix = &text[start..end];
    // Anchor the slice on a month-name prefix; a bare first-letter
    // scan picks up arbitrary trailing letters from the prompt body
    // ("Official Jun…" → "ial Jun…") and fails the timestamp parse.
    const MONTHS: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut best: Option<usize> = None;
    for m in MONTHS {
        if let Some(i) = prefix.find(m) {
            best = Some(best.map(|b| b.min(i)).unwrap_or(i));
        }
    }
    let first_letter = best?;
    Some(prefix[first_letter..].to_string())
}

pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut last_was_space = false;
    for c in s.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                if !last_was_space && !out.is_empty() {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            continue;
        }
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    decode_entities(out.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<html><body>
<div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
  <div class="content-cell">
    Watched <a href="https://www.youtube.com/watch?v=abc123">Make it so</a><br>
    <a href="https://www.youtube.com/channel/UC123">Captain Picard</a><br>
    Jun 4, 2026, 11:48:37 AM PDT
  </div>
</div>
<div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
  <div class="content-cell">
    Watched <a href="https://www.youtube.com/watch?v=def456">Engage!</a><br>
    <a href="https://www.youtube.com/channel/UC456">William Riker</a><br>
    Jun 5, 2026, 9:00:00 AM PDT
  </div>
</div>
</body></html>"#;

    #[test]
    fn iter_cells_finds_two() {
        let cells: Vec<&str> = iter_cells(SAMPLE).collect();
        assert_eq!(cells.len(), 2);
        assert!(cells[0].contains("Make it so"));
        assert!(cells[1].contains("Engage"));
    }

    #[test]
    fn anchors_in_cell() {
        let cells: Vec<&str> = iter_cells(SAMPLE).collect();
        let anchors = iter_anchors(cells[0]);
        assert_eq!(anchors.len(), 2);
        assert!(anchors[0].0.contains("watch?v=abc123"));
        assert_eq!(anchors[0].1, "Make it so");
        assert!(anchors[1].0.contains("/channel/UC123"));
        assert_eq!(anchors[1].1, "Captain Picard");
    }

    #[test]
    fn last_timestamp_chunk_strips_tags() {
        let cells: Vec<&str> = iter_cells(SAMPLE).collect();
        let ts = last_timestamp_chunk(cells[1]).expect("should find ts");
        assert!(ts.contains("Jun 5, 2026"));
        assert!(ts.contains("9:00:00 AM PDT"));
    }

    /// A multi-byte character 30 bytes before " AM " once panicked the
    /// whole Takeout ingest: the look-back sliced through the middle of it.
    #[test]
    fn last_timestamp_chunk_survives_a_multibyte_char_in_the_look_back() {
        let cell = "<div>Watched Worf's Brüder 1234 Jun 5, 2026, 9:00:00 AM PDT</div>";
        let text = strip_tags(cell);
        let look_back = text.find(" AM ").unwrap() - 30;
        assert!(
            !text.is_char_boundary(look_back),
            "the fixture must put the look-back inside the 'ü'"
        );
        assert_eq!(
            last_timestamp_chunk(cell).as_deref(),
            Some("Jun 5, 2026, 9:00:00 AM PDT")
        );
    }

    #[test]
    fn decode_entities_handles_named_and_numeric_references() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(
            decode_entities("I&#39;ve &quot;seen&quot; &#x2014; &lt;it&gt;"),
            "I've \"seen\" \u{2014} <it>"
        );
        assert_eq!(decode_entities("&amp;#39;"), "&#39;", "decoded once");
        assert_eq!(decode_entities("Q & A &bogus; &"), "Q & A &bogus; &");
    }

    #[test]
    fn div_contents_spans_nested_divs() {
        let html = r#"<div class="x"><div class="a">one<div>two</div></div><div>three</div></div>"#;
        assert_eq!(
            div_contents(html, r#"class="a">"#),
            Some("one<div>two</div>")
        );
        assert_eq!(div_contents(html, "nowhere"), None);
    }

    #[test]
    fn img_srcs_are_read_from_each_tag() {
        assert_eq!(
            iter_img_srcs(r#"<p><img alt="" src="a&amp;b.png"></p><img src="c.jpg" class="p">"#),
            ["a&b.png", "c.jpg"]
        );
    }
}
