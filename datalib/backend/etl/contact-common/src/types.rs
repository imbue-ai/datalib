//! What [`crate::render`] takes: a `DatalibContact` — who the person is —
//! and what rendering it as a document needs besides, which is the
//! render framework's business and never part of the person.

use datalib_contact_schema::DatalibContact;

#[derive(Debug, Clone)]
pub struct ContactDoc {
    pub contact: DatalibContact,
    /// The document's id: its `markdown_uuid` and grid row `uuid`,
    /// minted by the provider through `datalib_id` from `contact.key`.
    pub doc_uuid: String,
    /// The address book, or LinkedIn's single "connections" list, the
    /// card is filed in: the grid's `conversation_uuid` and `channel`.
    pub group_uuid: String,
    pub group_label: String,
    /// The account the id was minted under, for
    /// `grid_rows.upstream_account`; `None` when the record names none.
    pub upstream_account: Option<String>,
    /// Every raw row the card was built from, for incrementality.
    pub inputs: Vec<datalib_etl_render::inputs::Input>,
}
