//! What [`crate::render`] takes: a `NormalizedContact` — who the person is —
//! and what rendering it as a document needs besides, which is the
//! render framework's business and never part of the person.

use datalib_contact_schema::NormalizedContact;
use datalib_handle::Handle;

#[derive(Debug, Clone)]
pub struct ContactDoc {
    pub contact: NormalizedContact,
    /// The document's id: its `markdown_uuid` and grid row `uuid`,
    /// minted by the provider through `datalib_id` from `contact.key`.
    pub doc_uuid: String,
    /// The address book, or LinkedIn's single "connections" list, the
    /// card is filed in: the grid's `channel`, and nowhere else.
    pub group_label: String,
    /// For a group, the handle each of `contact.members` is drawn by (in
    /// its order, `None` where the member has none), so its page lists
    /// them as chips. Empty for a person.
    pub member_handles: Vec<Option<Handle>>,
    /// The account the id was minted under, for
    /// `grid_rows.upstream_account`; `None` when the record names none.
    pub upstream_account: Option<String>,
    /// Every raw row the card was built from, for incrementality.
    pub inputs: Vec<datalib_etl_render::inputs::Input>,
}
