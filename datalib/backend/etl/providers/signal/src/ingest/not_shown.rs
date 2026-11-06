//! What a Signal backup holds that no page shows. Every chat item's frame is
//! stored whole, so nothing here is lost from the mirror; it is absent from
//! search and from the transcript. One row, since a real backup holds thousands.

use std::collections::BTreeMap;

use datalib_etl::download_problems::SkippedRecord;
use datalib_problems::{Problem, Reason, Severity};
use datalib_signal_backup::backup;

/// The part of a download's `skipped:` rows this row is.
pub const PART: &str = "not_shown";

#[derive(Debug, Default)]
pub struct NotShown {
    counts: BTreeMap<&'static str, usize>,
}

impl NotShown {
    fn add(&mut self, what: &'static str, n: usize) {
        if n > 0 {
            *self.counts.entry(what).or_default() += n;
        }
    }

    pub fn chat_item(&mut self, item: &backup::ChatItem) {
        use backup::chat_item::Item;
        self.add("edited messages", usize::from(!item.revisions.is_empty()));
        match &item.item {
            Some(Item::StandardMessage(m)) => {
                self.add("reactions", m.reactions.len());
                self.add("quotes", usize::from(m.quote.is_some()));
                self.add("link previews", m.link_preview.len());
                self.add(
                    "long messages cut short",
                    usize::from(m.long_text.is_some()),
                );
            }
            Some(Item::ContactMessage(_)) => self.add("shared contacts", 1),
            Some(Item::StickerMessage(_)) => self.add("stickers", 1),
            Some(Item::RemoteDeletedMessage(_) | Item::AdminDeletedMessage(_)) => {
                self.add("deleted messages", 1)
            }
            Some(Item::UpdateMessage(_)) => self.add("chat updates", 1),
            Some(Item::PaymentNotification(_)) => self.add("payments", 1),
            Some(Item::GiftBadge(_)) => self.add("gift badges", 1),
            Some(Item::ViewOnceMessage(_)) => self.add("view-once messages", 1),
            Some(Item::DirectStoryReplyMessage(_)) => self.add("story replies", 1),
            Some(Item::Poll(_)) => self.add("polls", 1),
            None => self.add("empty items", 1),
        }
    }

    pub fn frame(&mut self, item: &backup::frame::Item) {
        use backup::frame::Item;
        match item {
            Item::StickerPack(_) => self.add("sticker packs", 1),
            Item::AdHocCall(_) => self.add("call links", 1),
            Item::NotificationProfile(_) => self.add("notification profiles", 1),
            Item::ChatFolder(_) => self.add("chat folders", 1),
            Item::Account(_) | Item::Recipient(_) | Item::Chat(_) | Item::ChatItem(_) => {}
        }
    }

    pub fn problem(&self) -> Option<SkippedRecord> {
        if self.counts.is_empty() {
            return None;
        }
        let said: Vec<String> = self
            .counts
            .iter()
            .map(|(w, n)| format!("{w}: {n}"))
            .collect();
        Some(SkippedRecord {
            entry: PART.to_string(),
            problem: Problem::explained(
                Reason::DeliberateLoss,
                None,
                &format!(
                    "In the last backup read, stored and not shown: {}",
                    said.join(", ")
                ),
            )
            .severity(Severity::Warning),
        })
    }
}
