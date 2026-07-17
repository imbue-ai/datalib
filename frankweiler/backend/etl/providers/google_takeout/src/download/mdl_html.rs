//! Minimal MDL `outer-cell` walker, shared by `youtube_watch_history`
//! and `gemini_apps`.
//!
//! Takeout HTML exports are ~140 KB of inlined Material Design Lite
//! CSS followed by N copies of:
//!
//! ```html
//! <div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
//!   <div class="header-cell mdl-cell mdl-cell--12-col">…header…</div>
//!   <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">
//!     …row body — anchors, prompt text, timestamp…
//!   </div>
//!   <div class="content-cell mdl-cell mdl-cell--12-col mdl-typography--caption">
//!     …"Products: …", "Why is this here?" provenance…
//!   </div>
//! </div>
//! ```
//!
//! We pre-load the whole file (the file is at worst tens of MB in
//! practice — see `docs/dev/google_takeout_ingestion.md` § "Watch-history
//! (and Gemini Apps HTML) at scale" for the math) and walk it with
//! `str::find` for the `<div class="outer-cell` boundaries. No
//! `scraper` / `html5ever` dependency.

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
/// `(href, inner_text)`. The href and inner text are unescaped
/// only for `&amp;` (the one HTML entity Google's exporter actually
/// emits in these fields). Inner text is **NOT** stripped of nested
/// tags — for MDL cells the anchor contents are bare strings, so
/// the simple `<a>(.*?)</a>` matches what's there.
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
        let href = decode_minimal(&cell[href_start..href_close]);
        // Body runs from the close of the opening tag to the next `</a>`.
        let Some(open_close_rel) = cell[href_close..].find('>') else {
            break;
        };
        let body_start = href_close + open_close_rel + 1;
        let Some(close_rel) = cell[body_start..].find("</a>") else {
            break;
        };
        let body_end = body_start + close_rel;
        let body = decode_minimal(&cell[body_start..body_end]);
        out.push((href, body));
        cursor = body_end + "</a>".len();
    }
    out
}

/// Decode the two HTML entities Google's MDL exporter actually emits
/// in the fields we care about (`&amp;`, `&quot;`). Anything else
/// passes through unchanged.
pub fn decode_minimal(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"")
}

/// Find the last "Month Day, Year, HH:MM:SS AM/PM TZ" chunk inside
/// the cell — Google appends the timestamp at the very end of the
/// entry's body cell, after the anchors / prompt text. Returns the
/// raw timestamp string (not parsed); the caller routes it through
/// [`super::time::parse_mdl_grid`].
///
/// We scan backwards for the last `AM`/`PM` token (so an earlier
/// mention of "AM" in the prompt text doesn't win) and slice a
/// generous prefix so the parser sees the full timestamp.
pub fn last_timestamp_chunk(cell: &str) -> Option<String> {
    let text = strip_tags(cell);
    let mut ampm_idx: Option<usize> = None;
    for marker in [" AM ", " PM "] {
        if let Some(i) = text.rfind(marker) {
            ampm_idx = Some(ampm_idx.map(|x| x.max(i)).unwrap_or(i));
        }
    }
    let ampm = ampm_idx?;
    let start = ampm.saturating_sub(30);
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

/// Strip every `<…>` tag from `s`, collapsing surrounding whitespace
/// to one space. Good enough for the MDL cell shape where tag
/// content is structured (no `<script>` / `<style>` mid-cell).
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
    decode_minimal(out.trim())
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

    #[test]
    fn decode_minimal_handles_amp() {
        assert_eq!(decode_minimal("a &amp; b"), "a & b");
    }
}
