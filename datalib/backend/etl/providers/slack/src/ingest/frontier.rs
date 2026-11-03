//! The order a run fetches in: newest first, across every conversation
//! it mirrors. Each channel's history walks wait between pages, and the
//! threads and files the store owes wait in queues by how new they are;
//! the next request is always for the newest thing not yet asked for.
//! Everything here is a decision over values; `mod.rs` acts on it.

use std::collections::{BTreeSet, HashMap};

use datalib_etl_web::owed::Listed;

use super::{Stretch, END_OF_TIME};

/// A walk of one [`Stretch`] of one channel, between two pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Walk {
    /// The channel, as its place in the run's targets.
    pub target: usize,
    pub stretch: Stretch,
    pub cursor: Option<String>,
    /// Where the next page reaches down from: the stretch's own top, then
    /// each page's oldest message. `None` until the first page of an
    /// open-topped stretch says where its newest message is.
    pub top: Option<String>,
    /// Whether a message at `top` is the next page's to return. The
    /// stretch's own top is; a page's oldest message was that page's.
    pub top_included: bool,
    /// A page has been asked for and not yet answered.
    pub asking: bool,
    /// Its last page is in, or its channel failed.
    pub ended: bool,
}

impl Walk {
    pub fn new(target: usize, stretch: Stretch) -> Self {
        let top = (stretch.span.hi != END_OF_TIME).then(|| stretch.span.hi.clone());
        Self {
            target,
            stretch,
            cursor: None,
            top,
            top_included: true,
            asking: false,
            ended: false,
        }
    }

    /// The newest a message on the next page can be, as a `ts` key.
    pub fn reach(&self) -> &str {
        self.top.as_deref().unwrap_or(END_OF_TIME)
    }
}

/// Owed records, each at the `ts` key of the newest message fetching it
/// brings: a thread at its latest reply, a file at its message.
#[derive(Debug, Default)]
pub(super) struct Queue {
    order: BTreeSet<(String, String)>,
    by_key: HashMap<String, (String, Listed)>,
}

impl Queue {
    /// Owe `listed` at `reach`, in place of wherever it was owed before:
    /// a thread listed again with a newer reply moves up. Whether it is
    /// newly owed, as against moved.
    pub fn push(&mut self, reach: String, listed: Listed) -> bool {
        let moved = self.by_key.remove(&listed.key);
        if let Some((was, _)) = &moved {
            self.order.remove(&(was.clone(), listed.key.clone()));
        }
        self.order.insert((reach.clone(), listed.key.clone()));
        self.by_key.insert(listed.key.clone(), (reach, listed));
        moved.is_none()
    }

    /// Up to `n` of what is owed at `reach` or newer, newest first.
    pub fn take_down_to(&mut self, reach: &str, n: usize) -> Vec<Listed> {
        let floor = (reach.to_string(), String::new());
        let taken: Vec<(String, String)> =
            self.order.range(floor..).rev().take(n).cloned().collect();
        taken
            .into_iter()
            .filter_map(|entry| {
                self.order.remove(&entry);
                self.by_key.remove(&entry.1).map(|(_, listed)| listed)
            })
            .collect()
    }
}

/// What a run asks for next.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    Thread(Listed),
    /// Files for one request, newest first.
    Files(Vec<Listed>),
    /// The next page of `walks[i]`, which is now [`Walk::asking`].
    Page(usize),
    /// Nothing until something asked for answers, which may owe more.
    Nothing,
}

#[derive(Debug, Default)]
pub(super) struct Frontier {
    pub walks: Vec<Walk>,
    pub threads: Queue,
    pub files: Queue,
}

impl Frontier {
    /// A thread, then files, owed at or above the newest page a walk can
    /// ask for now; with none, that page. A page's messages are at most
    /// its reach, so nothing it could bring is newer than what is asked
    /// for first. A channel with a page out asks for nothing more of its
    /// history until that page answers: the next page's cursor comes
    /// from it. With nothing out, as with one request at a time, this is
    /// the newest of everything owed.
    pub fn next(&mut self, files_per_request: usize) -> Next {
        let page = self.newest_walk();
        let reach = page.map_or("", |i| self.walks[i].reach()).to_string();
        if let Some(thread) = self.threads.take_down_to(&reach, 1).pop() {
            return Next::Thread(thread);
        }
        let files = self.files.take_down_to(&reach, files_per_request.max(1));
        if !files.is_empty() {
            return Next::Files(files);
        }
        match page {
            Some(i) => {
                self.walks[i].asking = true;
                Next::Page(i)
            }
            None => Next::Nothing,
        }
    }

    /// Of the walks that can ask now and reach newest, the first:
    /// channels that tie, as every one does before its first page, go in
    /// the order listed.
    fn newest_walk(&self) -> Option<usize> {
        let mut newest: Option<usize> = None;
        for (i, walk) in self.walks.iter().enumerate() {
            if walk.ended || self.asking(walk.target) {
                continue;
            }
            if newest.is_none_or(|n| walk.reach() > self.walks[n].reach()) {
                newest = Some(i);
            }
        }
        newest
    }

    fn asking(&self, target: usize) -> bool {
        self.walks.iter().any(|w| w.target == target && w.asking)
    }

    /// `walks[i]` has its answer; `last` when that was its last page.
    pub fn answered(&mut self, i: usize, last: bool) {
        self.walks[i].asking = false;
        self.walks[i].ended |= last;
    }

    /// `target` asks for no more history this run.
    pub fn end(&mut self, target: usize) {
        for walk in self.walks.iter_mut().filter(|w| w.target == target) {
            walk.asking = false;
            walk.ended = true;
        }
    }

    /// Whether `target` has a walk left.
    pub fn walking(&self, target: usize) -> bool {
        self.walks.iter().any(|w| w.target == target && !w.ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_web::coverage::Span;

    const FILES: usize = 8;

    fn open(target: usize) -> Walk {
        Walk::new(
            target,
            Stretch {
                span: Span::new("0", END_OF_TIME),
                inclusive: true,
                reread: false,
            },
        )
    }

    fn at(target: usize, top: &str) -> Walk {
        Walk {
            top: Some(top.to_string()),
            top_included: false,
            ..open(target)
        }
    }

    fn listed(key: &str) -> Listed {
        Listed::new(key, None::<String>)
    }

    fn keys(listed: &[Listed]) -> Vec<&str> {
        listed.iter().map(|l| l.key.as_str()).collect()
    }

    /// Ask for `walks[i]`'s next page and answer it, its oldest message
    /// at `oldest`.
    fn page_answered(f: &mut Frontier, i: usize, oldest: &str) {
        assert_eq!(f.next(FILES), Next::Page(i));
        f.walks[i].top = Some(oldest.into());
        f.answered(i, false);
    }

    #[test]
    fn every_channel_answers_its_first_page_before_any_channel_its_second() {
        let mut f = Frontier {
            walks: vec![at(0, "5"), open(1), open(2)],
            ..Default::default()
        };
        page_answered(&mut f, 1, "3");
        page_answered(&mut f, 2, "7");
        assert_eq!(f.next(FILES), Next::Page(2));
    }

    #[test]
    fn a_thread_newer_than_every_page_left_comes_before_them() {
        let mut f = Frontier {
            walks: vec![at(0, "5")],
            ..Default::default()
        };
        f.threads.push("6".into(), listed("new"));
        f.threads.push("4".into(), listed("old"));
        f.files.push("5".into(), listed("file"));
        assert_eq!(f.next(FILES), Next::Thread(listed("new")));
        assert_eq!(f.next(FILES), Next::Files(vec![listed("file")]));
        assert_eq!(f.next(FILES), Next::Page(0));
        f.answered(0, true);
        assert_eq!(f.next(FILES), Next::Thread(listed("old")));
        assert_eq!(f.next(FILES), Next::Nothing);
    }

    /// With requests out at once, the next one is the newest of what is
    /// not already asked for: a channel with a page out waits for it,
    /// and what is owed below that page is asked for meanwhile.
    #[test]
    fn a_channel_with_a_page_out_waits_and_the_rest_go_on_in_order() {
        let mut f = Frontier {
            walks: vec![at(0, "9"), at(1, "5"), at(1, "2")],
            ..Default::default()
        };
        f.threads.push("7".into(), listed("seven"));
        f.threads.push("3".into(), listed("three"));
        assert_eq!(f.next(FILES), Next::Page(0));
        assert_eq!(f.next(FILES), Next::Thread(listed("seven")));
        assert_eq!(f.next(FILES), Next::Page(1));
        assert_eq!(
            f.next(FILES),
            Next::Thread(listed("three")),
            "channel 1's other stretch waits for the page it has out"
        );
        assert_eq!(f.next(FILES), Next::Nothing);
        f.answered(1, true);
        assert_eq!(f.next(FILES), Next::Page(2));
        f.end(0);
        assert!(!f.walking(0));
        assert!(f.walking(1));
    }

    #[test]
    fn a_queue_hands_back_newest_first_and_a_record_owed_again_moves() {
        let mut q = Queue::default();
        assert!(q.push("2".into(), listed("a")));
        assert!(q.push("3".into(), listed("b")));
        assert!(q.push("1".into(), listed("c")));
        assert!(!q.push("4".into(), listed("a")), "moved, not newly owed");
        assert_eq!(keys(&q.take_down_to("2", 1)), ["a"]);
        assert_eq!(keys(&q.take_down_to("2", FILES)), ["b"]);
        assert_eq!(keys(&q.take_down_to("", FILES)), ["c"]);
        assert!(q.take_down_to("", FILES).is_empty());
        assert!(q.push("2".into(), listed("a")), "owed again once taken");
    }
}
