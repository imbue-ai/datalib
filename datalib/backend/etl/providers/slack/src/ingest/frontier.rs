//! The order a run fetches in: newest first, across every conversation
//! it mirrors. Each channel's history walks wait between pages, and the
//! threads and files the store owes wait in queues by how new they are;
//! the next request is always for the newest thing not yet fetched.
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
    /// a thread listed again with a newer reply moves up.
    pub fn push(&mut self, reach: String, listed: Listed) {
        if let Some((was, _)) = self.by_key.remove(&listed.key) {
            self.order.remove(&(was, listed.key.clone()));
        }
        self.order.insert((reach.clone(), listed.key.clone()));
        self.by_key.insert(listed.key.clone(), (reach, listed));
    }

    /// Everything owed at `reach` or newer, newest first.
    pub fn take_down_to(&mut self, reach: &str) -> Vec<Listed> {
        let newer = self.order.split_off(&(reach.to_string(), String::new()));
        newer
            .into_iter()
            .rev()
            .filter_map(|(_, key)| self.by_key.remove(&key).map(|(_, listed)| listed))
            .collect()
    }
}

/// What a run does next.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    /// Fetch these threads, newest first.
    Threads(Vec<Listed>),
    /// Fetch these files, newest first.
    Files(Vec<Listed>),
    /// Ask for the next page of `walks[i]`.
    Page(usize),
    Done,
}

#[derive(Debug, Default)]
pub(super) struct Frontier {
    pub walks: Vec<Walk>,
    pub threads: Queue,
    pub files: Queue,
}

impl Frontier {
    /// The threads, then the files, owed at or above the newest page any
    /// walk has left to ask for; with none, that page. A page's
    /// messages are at most its reach, so nothing it could bring is
    /// newer than what is fetched first.
    pub fn next(&mut self) -> Next {
        let page = self.newest_walk();
        let reach = page.map_or("", |i| self.walks[i].reach()).to_string();
        let threads = self.threads.take_down_to(&reach);
        if !threads.is_empty() {
            return Next::Threads(threads);
        }
        let files = self.files.take_down_to(&reach);
        if !files.is_empty() {
            return Next::Files(files);
        }
        page.map_or(Next::Done, Next::Page)
    }

    /// Of the walks that reach newest, the first: channels that tie, as
    /// every one does before its first page, go in the order listed.
    fn newest_walk(&self) -> Option<usize> {
        let mut newest: Option<usize> = None;
        for (i, walk) in self.walks.iter().enumerate() {
            if newest.is_none_or(|n| walk.reach() > self.walks[n].reach()) {
                newest = Some(i);
            }
        }
        newest
    }

    /// Whether `target` has a walk left.
    pub fn walking(&self, target: usize) -> bool {
        self.walks.iter().any(|w| w.target == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_web::coverage::Span;

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

    #[test]
    fn every_channel_answers_its_first_page_before_any_channel_its_second() {
        let mut f = Frontier {
            walks: vec![at(0, "5"), open(1), open(2)],
            ..Default::default()
        };
        assert_eq!(f.next(), Next::Page(1));
        f.walks[1].top = Some("3".into());
        assert_eq!(f.next(), Next::Page(2));
        f.walks[2].top = Some("7".into());
        assert_eq!(f.next(), Next::Page(2));
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
        assert_eq!(f.next(), Next::Threads(vec![listed("new")]));
        assert_eq!(f.next(), Next::Files(vec![listed("file")]));
        assert_eq!(f.next(), Next::Page(0));
        f.walks.clear();
        assert_eq!(f.next(), Next::Threads(vec![listed("old")]));
        assert_eq!(f.next(), Next::Done);
    }

    #[test]
    fn a_queue_hands_back_newest_first_and_a_record_owed_again_moves() {
        let mut q = Queue::default();
        q.push("2".into(), listed("a"));
        q.push("3".into(), listed("b"));
        q.push("1".into(), listed("c"));
        q.push("4".into(), listed("a"));
        assert_eq!(keys(&q.take_down_to("2")), ["a", "b"]);
        assert_eq!(keys(&q.take_down_to("")), ["c"]);
        assert!(q.take_down_to("").is_empty());
    }
}
