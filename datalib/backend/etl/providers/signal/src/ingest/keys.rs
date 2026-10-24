//! The keys the raw store files recipients, chats and messages under.
//!
//! A backup numbers its recipients and chats itself (`Recipient.id`,
//! `Chat.id`), and a phone restored or transferred from another numbers
//! them anew, so a key built from those numbers names a different row
//! for the same message after a phone switch. These keys are built from
//! who a recipient is instead: the same person, group or chat gets the
//! same key in every backup of the account, whichever phone wrote it.

use std::collections::{HashMap, HashSet};

use datalib_signal_backup::backup;

/// Who a recipient is, on any phone; `None` for one with nothing stable
/// to go by.
///
/// A contact is known by their ACI, else their PNI, else their number.
/// A contact first seen by number alone and later with an ACI changes
/// key once, the backup that first carries the ACI. A group's and a call
/// link's keys are a hash of their secret, never the secret itself.
pub fn recipient_key(r: &backup::Recipient) -> Option<String> {
    use backup::recipient::Destination;
    match r.destination.as_ref()? {
        Destination::Self_(_) => Some("self".to_string()),
        Destination::ReleaseNotes(_) => Some("release_notes".to_string()),
        Destination::Contact(c) => {
            if let Some(aci) = c.aci.as_deref().filter(|b| !b.is_empty()) {
                Some(format!("aci:{}", hex(aci)))
            } else if let Some(pni) = c.pni.as_deref().filter(|b| !b.is_empty()) {
                Some(format!("pni:{}", hex(pni)))
            } else {
                c.e164.filter(|&n| n != 0).map(|n| format!("e164:+{n}"))
            }
        }
        Destination::Group(g) => secret_hash(&g.master_key).map(|h| format!("group:{h}")),
        Destination::DistributionList(d) => {
            (!d.distribution_id.is_empty()).then(|| format!("list:{}", hex(&d.distribution_id)))
        }
        Destination::CallLink(c) => secret_hash(&c.root_key).map(|h| format!("call_link:{h}")),
    }
}

/// The keys of one backup's recipients and chats, by the numbers that
/// backup gave them, built up as its frames are read.
///
/// A backup lists every recipient before a chat names it and every chat
/// before an item does. A number it never listed, a recipient with
/// nothing stable, and the second of two that would share a key are
/// keyed by their number (`recipient:7`, `chat:12`), which holds for this
/// backup only.
#[derive(Default)]
pub struct Keys {
    recipients: HashMap<u64, String>,
    chats: HashMap<u64, String>,
    taken: HashSet<String>,
}

impl Keys {
    pub fn add_recipient(&mut self, r: &backup::Recipient) -> String {
        let key = recipient_key(r)
            .filter(|k| self.taken.insert(k.clone()))
            .unwrap_or_else(|| format!("recipient:{}", r.id));
        self.recipients.insert(r.id, key.clone());
        key
    }

    /// A chat is keyed by the recipient it is with: Signal keeps one chat
    /// per recipient.
    pub fn add_chat(&mut self, c: &backup::Chat) -> String {
        let key = Some(self.recipient(c.recipient_id))
            .filter(|k| !k.starts_with("recipient:"))
            .filter(|k| self.taken.insert(format!("chat {k}")))
            .unwrap_or_else(|| format!("chat:{}", c.id));
        self.chats.insert(c.id, key.clone());
        key
    }

    pub fn recipient(&self, id: u64) -> String {
        self.recipients
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("recipient:{id}"))
    }

    pub fn chat(&self, id: u64) -> String {
        self.chats
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("chat:{id}"))
    }
}

/// 128 bits of blake3 is plenty to tell a few thousand groups apart, and
/// keeps a message key under the 96 characters `problems.scope_key` holds.
fn secret_hash(secret: &[u8]) -> Option<String> {
    (!secret.is_empty()).then(|| hex(&blake3::hash(secret).as_bytes()[..16]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use backup::recipient::Destination;

    fn contact(id: u64, aci: Option<u8>, e164: Option<u64>) -> backup::Recipient {
        backup::Recipient {
            id,
            destination: Some(Destination::Contact(backup::Contact {
                aci: aci.map(|last| {
                    let mut b = vec![0u8; 16];
                    b[15] = last;
                    b
                }),
                e164,
                ..Default::default()
            })),
        }
    }

    fn group(id: u64, master_key: &[u8]) -> backup::Recipient {
        backup::Recipient {
            id,
            destination: Some(Destination::Group(backup::Group {
                master_key: master_key.to_vec(),
                ..Default::default()
            })),
        }
    }

    fn me(id: u64) -> backup::Recipient {
        backup::Recipient {
            id,
            destination: Some(Destination::Self_(backup::Self_::default())),
        }
    }

    fn chat(id: u64, recipient_id: u64) -> backup::Chat {
        backup::Chat {
            id,
            recipient_id,
            ..Default::default()
        }
    }

    /// A new phone numbers its recipients and chats anew, and every
    /// message's key changed with them: the switch read as every message
    /// deleted and written again. Two backups that number the same people
    /// differently give them the same keys.
    #[test]
    fn two_backups_numbering_the_same_people_differently_key_them_alike() {
        let old_phone = {
            let mut k = Keys::default();
            k.add_recipient(&me(1));
            k.add_recipient(&contact(2, Some(0xa), Some(17015550101)));
            k.add_recipient(&group(3, b"bridge officers"));
            k.add_chat(&chat(100, 2));
            k.add_chat(&chat(101, 3));
            [k.recipient(1), k.recipient(2), k.chat(100), k.chat(101)]
        };
        let new_phone = {
            let mut k = Keys::default();
            k.add_recipient(&group(7, b"bridge officers"));
            k.add_recipient(&contact(8, Some(0xa), Some(17015550101)));
            k.add_recipient(&me(9));
            k.add_chat(&chat(1, 8));
            k.add_chat(&chat(2, 7));
            [k.recipient(9), k.recipient(8), k.chat(1), k.chat(2)]
        };
        assert_eq!(old_phone, new_phone);
        assert_eq!(old_phone[0], "self");
        assert_eq!(old_phone[1], "aci:0000000000000000000000000000000a");
        assert_eq!(
            old_phone[1], old_phone[2],
            "a chat is keyed by who it is with"
        );
        assert!(old_phone[3].starts_with("group:"));
        assert!(
            !old_phone[3].contains(&hex(b"bridge officers")),
            "a group's key does not hold its master key"
        );
    }

    /// A contact without an ACI is known by their number; one with
    /// nothing at all, or a second that would take a key already taken,
    /// keeps the backup's own number rather than sharing a row.
    #[test]
    fn a_recipient_without_a_stable_key_keeps_the_backups_number() {
        let mut k = Keys::default();
        assert_eq!(
            k.add_recipient(&contact(2, None, Some(17015550101))),
            "e164:+17015550101"
        );
        assert_eq!(k.add_recipient(&contact(3, None, None)), "recipient:3");
        assert_eq!(
            k.add_recipient(&contact(4, None, Some(17015550101))),
            "recipient:4"
        );
        assert_eq!(k.add_chat(&chat(10, 3)), "chat:10");
        assert_eq!(k.add_chat(&chat(11, 2)), "e164:+17015550101");
        assert_eq!(k.add_chat(&chat(12, 2)), "chat:12");
        assert_eq!(k.chat(99), "chat:99");
        assert_eq!(k.recipient(99), "recipient:99");
    }
}
