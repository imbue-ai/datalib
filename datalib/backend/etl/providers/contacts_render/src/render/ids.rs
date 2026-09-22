//! Contacts entity ids. A vCard `UID` is unique within its addressbook
//! and an addressbook's label within its account, and the CardDAV
//! principal is not extracted, so the ids are scoped to the configured
//! source. No stamp anywhere: a card carries no creation event.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Contacts;

pub const KIND_CONTACT: &str = "contact";
pub const KIND_ADDRESSBOOK: &str = "addressbook";

fn identity(source_id: &str, entity_kind: &'static str, natural_key: String) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        Scope::SourceInstance(source_id),
        entity_kind,
        natural_key,
        None,
    )
}

/// vCards with the same UID under the same addressbook collapse into
/// one contact, whether they came from a sync-collection REPORT or a
/// `.vcf` file on disk.
pub fn contact(source_id: &str, addressbook_label: &str, uid: &str) -> Identity {
    identity(
        source_id,
        KIND_CONTACT,
        composite_key(&[addressbook_label, uid]),
    )
}

pub fn addressbook(source_id: &str, addressbook_label: &str) -> Identity {
    identity(source_id, KIND_ADDRESSBOOK, addressbook_label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::entity_id_str;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            contact("c", "Personal", "uid-1"),
            addressbook("c", "Personal"),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    Scope::SourceInstance("c"),
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn addressbooks_and_sources_separate_one_uid() {
        let a = contact("c", "Personal", "uid-1");
        assert_eq!(a.uuid, contact("c", "Personal", "uid-1").uuid);
        assert_ne!(a.uuid, contact("c", "Work", "uid-1").uuid);
        assert_ne!(a.uuid, contact("d", "Personal", "uid-1").uuid);
        assert_ne!(a.uuid, addressbook("c", "Personal").uuid);
    }
}
