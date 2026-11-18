//! Email entity ids. Every method — JMAP, the Gmail API, an mbox —
//! carries an account id on every row, and a thread or email id is
//! unique within that account, so the scope is the account.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Email;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_THREAD: &str = "thread";
pub const KIND_EMAIL: &str = "email";
pub const KIND_ACCOUNT: &str = "account";
pub const KIND_MAILBOX: &str = "mailbox";
/// A label's parent that exists only as the part of its name before a
/// `/`: no mailbox row of its own, so its path is its key.
pub const KIND_LABEL_PATH: &str = "label_path";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

pub fn thread(source_id: &str, account_id: &str, thread_id: &str) -> Identity {
    IDS.mint_in(
        source_id,
        account_id,
        KIND_THREAD,
        thread_id.to_string(),
        None,
    )
}

pub fn account(source_id: &str, account_id: &str) -> Identity {
    IDS.mint_in(
        source_id,
        account_id,
        KIND_ACCOUNT,
        account_id.to_string(),
        None,
    )
}

/// `mailbox_id` is the `mailboxes` row's id: JMAP's own, Gmail's label
/// id, or a name-keyed one (`labels::mailbox_id`).
pub fn mailbox(source_id: &str, account_id: &str, mailbox_id: &str) -> Identity {
    IDS.mint_in(
        source_id,
        account_id,
        KIND_MAILBOX,
        mailbox_id.to_string(),
        None,
    )
}

pub fn label_path(source_id: &str, account_id: &str, path: &str) -> Identity {
    IDS.mint_in(
        source_id,
        account_id,
        KIND_LABEL_PATH,
        path.to_string(),
        None,
    )
}

/// `date_ms` is the email's `received_at` as the item stores it.
pub fn email(source_id: &str, account_id: &str, email_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint_in(
        source_id,
        account_id,
        KIND_EMAIL,
        email_id.to_string(),
        date_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            account("mail", "acct"),
            mailbox("mail", "acct", "M1"),
            label_path("mail", "acct", "Work/Clients"),
            thread("mail", "acct", "t1"),
            email("mail", "acct", "e1", Some(1_700_000_000_999)),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "mail",
                    Some("acct"),
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn accounts_are_separated_and_kinds_too() {
        assert_ne!(thread("mail", "a", "x").uuid, thread("mail", "b", "x").uuid);
        assert_ne!(
            thread("mail", "a", "x").uuid,
            email("mail", "a", "x", None).uuid
        );
    }

    #[test]
    fn an_email_carries_its_stamp_to_the_second_and_a_thread_none() {
        assert_eq!(
            stamp_of(&email("mail", "a", "e", Some(1_700_000_000_999)).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&thread("mail", "a", "t").uuid), None);
    }
}
