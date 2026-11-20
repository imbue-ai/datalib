//! Which of a document's pieces a page of it holds, in the document's own
//! order: the order render handed its sections over in, which is how the
//! document reads (a version of a conversation where it forked, a run of
//! tool steps where it happened), not the order of its messages' times.
//! A page is a run of the document's top-level nodes, a fold whole, so a
//! page never ends inside one. Pure: the store reads the outline and then
//! the pieces a window names
//! (`docs/dev/plans/document_viewer_from_blocks.md` §"Step 6").

use datalib_schema::folds::FoldRow;

/// One top-level node of a document, as the places (`seq`) of its first
/// and last piece: a piece alone, or a fold with everything inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub first: i64,
    pub last: i64,
}

/// The top-level nodes of a document of `len` pieces whose folds are
/// `folds`, in order. A close with no fold open is a piece of its own, and
/// a fold left open runs to the end, as [`crate::section::nodes`] reads
/// them.
pub fn spans(len: i64, folds: &[FoldRow]) -> Vec<Span> {
    let opens: std::collections::HashMap<i64, bool> =
        folds.iter().map(|f| (f.seq, f.class.is_some())).collect();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut first = 0;
    for seq in 0..len {
        if depth == 0 {
            first = seq;
        }
        match opens.get(&seq) {
            Some(true) => depth += 1,
            Some(false) if depth > 0 => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            out.push(Span { first, last: seq });
        }
    }
    if depth > 0 {
        out.push(Span {
            first,
            last: len - 1,
        });
    }
    out
}

/// Where a page of a document starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The first page.
    Start,
    /// The page around the node holding the piece at this place.
    Around(i64),
    /// The page from this cursor on.
    After(i64),
    /// The page that ends just before this cursor.
    Before(i64),
}

/// A window of top-level nodes, and the cursors either side of it: the
/// place of the first piece of the node before it ends (`before`), and of
/// the node after it (`after`). `None` where the window reaches that end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub first: i64,
    pub last: i64,
    pub before: Option<i64>,
    pub after: Option<i64>,
}

/// The window of at most `limit` nodes `ask` names, `None` when it names
/// none: a cursor past either end, or a document with no pieces.
pub fn window(spans: &[Span], ask: Ask, limit: usize) -> Option<Window> {
    let n = spans.len();
    let limit = limit.max(1);
    let (lo, hi) = match ask {
        Ask::Start => (0, limit.min(n).checked_sub(1)?),
        Ask::Around(seq) => {
            let at = spans.iter().position(|s| s.first <= seq && seq <= s.last)?;
            let lo = at.saturating_sub(limit / 2);
            let hi = (lo + limit - 1).min(n.checked_sub(1)?);
            (hi.saturating_sub(limit - 1).min(lo), hi)
        }
        Ask::After(cursor) => {
            let lo = spans.iter().position(|s| s.first >= cursor)?;
            (lo, (lo + limit - 1).min(n - 1))
        }
        Ask::Before(cursor) => {
            let hi = spans.iter().rposition(|s| s.last < cursor)?;
            (hi.saturating_sub(limit - 1), hi)
        }
    };
    Some(Window {
        first: spans[lo].first,
        last: spans[hi].last,
        before: (lo > 0).then(|| spans[lo].first),
        after: spans.get(hi + 1).map(|s| s.first),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(seq: i64, opens: bool) -> FoldRow {
        FoldRow {
            markdown_uuid: "doc".into(),
            seq,
            class: opens.then(|| "tool-group".into()),
            summary: opens.then(|| "🛠".into()),
        }
    }

    fn span(first: i64, last: i64) -> Span {
        Span { first, last }
    }

    /// A fold, with everything inside it and the folds it holds, is one
    /// node; every other piece is a node of its own.
    #[test]
    fn a_fold_is_one_node_however_deep() {
        // 0 title, 1 open, 2 msg, 3 open, 4 msg, 5 close, 6 close, 7 msg.
        let folds = [fold(1, true), fold(3, true), fold(5, false), fold(6, false)];
        assert_eq!(spans(8, &folds), [span(0, 0), span(1, 6), span(7, 7)]);
    }

    /// A close with nothing open is a piece; a fold left open runs to the
    /// end, as the nodes are read.
    #[test]
    fn a_stray_close_is_a_piece_and_an_open_fold_runs_to_the_end() {
        assert_eq!(
            spans(3, &[fold(0, false)]),
            [span(0, 0), span(1, 1), span(2, 2)]
        );
        assert_eq!(spans(3, &[fold(1, true)]), [span(0, 0), span(1, 2)]);
    }

    fn ten() -> Vec<Span> {
        (0..10).map(|s| span(s, s)).collect()
    }

    /// Paging from the start, then on and back by the cursors each page
    /// gives, visits every node once, and a document no longer than a page
    /// comes whole, with no cursor either side.
    #[test]
    fn pages_walk_the_document_both_ways() {
        let spans = ten();
        let first = window(&spans, Ask::Start, 4).unwrap();
        assert_eq!(
            (first.first, first.last, first.before, first.after),
            (0, 3, None, Some(4))
        );
        let second = window(&spans, Ask::After(first.after.unwrap()), 4).unwrap();
        assert_eq!((second.first, second.last), (4, 7));
        let third = window(&spans, Ask::After(second.after.unwrap()), 4).unwrap();
        assert_eq!((third.first, third.last, third.after), (8, 9, None));
        let back = window(&spans, Ask::Before(third.first), 4).unwrap();
        assert_eq!((back.first, back.last, back.before), (4, 7, Some(4)));
        let whole = window(&spans, Ask::Start, 10).unwrap();
        assert_eq!((whole.before, whole.after), (None, None));
    }

    /// A page around a piece holds it near its middle, and slides inward
    /// at either end rather than coming up short.
    #[test]
    fn a_page_around_a_piece_holds_it() {
        let spans = ten();
        let mid = window(&spans, Ask::Around(5), 4).unwrap();
        assert_eq!((mid.first, mid.last), (3, 6));
        let end = window(&spans, Ask::Around(9), 4).unwrap();
        assert_eq!((end.first, end.last, end.after), (6, 9, None));
        let start = window(&spans, Ask::Around(0), 4).unwrap();
        assert_eq!((start.first, start.last, start.before), (0, 3, None));
    }

    /// A piece inside a fold is found in the fold's node, and a page never
    /// ends inside one.
    #[test]
    fn a_page_never_ends_inside_a_fold() {
        let spans = spans(8, &[fold(1, true), fold(6, false)]);
        let w = window(&spans, Ask::Around(4), 1).unwrap();
        assert_eq!((w.first, w.last), (1, 6));
        let w = window(&spans, Ask::Start, 2).unwrap();
        assert_eq!((w.first, w.last, w.after), (0, 6, Some(7)));
    }

    /// A cursor past an end, or an empty document, names no page.
    #[test]
    fn nothing_past_the_ends() {
        let spans = ten();
        assert_eq!(window(&spans, Ask::After(10), 4), None);
        assert_eq!(window(&spans, Ask::Before(0), 4), None);
        assert_eq!(window(&[], Ask::Start, 4), None);
        assert_eq!(window(&spans, Ask::Around(99), 4), None);
    }
}
