//! Whether the account has read an item, decided the same way for every
//! chat source from the [`ReadMark`]s it keeps: in its own `read_marks`
//! table, or, for a mirror of an app's database, built from the app's
//! columns at render.

use std::cmp::Ordering;

pub use datalib_etl::read_marks::ReadMark;

/// The marks that bear on one item.
#[derive(Debug, Clone, Copy, Default)]
pub struct Marks<'a> {
    /// On the item itself. Only a [`ReadMark::Read`] means anything here.
    pub own: Option<&'a ReadMark>,
    /// On the conversation or thread the item is read in. Only a
    /// [`ReadMark::Through`] means anything here: a conversation marked
    /// unread as a whole does not say which of its items are.
    pub conversation: Option<&'a ReadMark>,
}

/// Whether the item keyed `key` is unread. The account's own items never
/// are: a flag on one is the recipient's receipt, not the account's
/// reading. Otherwise the item's own mark decides, being the more
/// specific; failing that, it is unread when `order` puts it after its
/// conversation's `Through` mark. `false` stands for read *and* for
/// cannot tell, so a key `order` cannot compare is not unread.
pub fn is_unread(
    key: &str,
    from_me: bool,
    marks: Marks<'_>,
    order: impl Fn(&str, &str) -> Option<Ordering>,
) -> bool {
    if from_me {
        return false;
    }
    if let Some(ReadMark::Read(read)) = marks.own {
        return !read;
    }
    match marks.conversation {
        Some(ReadMark::Through(last_read)) => order(key, last_read) == Some(Ordering::Greater),
        _ => false,
    }
}

/// Keys that are integers, a row id: compared as numbers, so `"10"`
/// comes after `"9"`.
pub fn integer_order(a: &str, b: &str) -> Option<Ordering> {
    Some(a.parse::<i64>().ok()?.cmp(&b.parse::<i64>().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unread(key: &str, from_me: bool, own: Option<ReadMark>, conv: Option<ReadMark>) -> bool {
        is_unread(
            key,
            from_me,
            Marks {
                own: own.as_ref(),
                conversation: conv.as_ref(),
            },
            integer_order,
        )
    }

    fn through(id: &str) -> Option<ReadMark> {
        Some(ReadMark::Through(id.into()))
    }

    /// An item is unread only past its conversation's mark, compared as
    /// the source orders its keys: by number, `10` after `9`.
    #[test]
    fn past_the_conversations_mark_is_unread() {
        assert!(!unread("8", false, None, through("9")));
        assert!(!unread("9", false, None, through("9")), "the mark itself");
        assert!(unread("10", false, None, through("9")), "not text order");
        assert!(!unread("x", false, None, through("9")), "cannot compare");
        assert!(!unread("10", false, None, None), "no mark: cannot tell");
    }

    /// An item's own mark is the more specific and wins over its
    /// conversation's, either way.
    #[test]
    fn an_items_own_mark_wins() {
        assert!(unread("1", false, Some(ReadMark::Read(false)), None));
        assert!(!unread("1", false, Some(ReadMark::Read(true)), None));
        assert!(unread(
            "1",
            false,
            Some(ReadMark::Read(false)),
            through("9")
        ));
        assert!(!unread(
            "10",
            false,
            Some(ReadMark::Read(true)),
            through("9")
        ));
    }

    /// A mark of the wrong kind for where it sits says nothing.
    #[test]
    fn a_mark_out_of_place_is_ignored() {
        assert!(!unread("10", false, through("9"), None));
        assert!(!unread("10", false, None, Some(ReadMark::Read(false))));
    }

    /// The account's own messages are never unread, whatever a mark says.
    #[test]
    fn own_messages_are_never_unread() {
        assert!(!unread("1", true, Some(ReadMark::Read(false)), None));
        assert!(!unread("10", true, None, through("9")));
    }
}
