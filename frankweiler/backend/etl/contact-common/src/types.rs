//! Provider-agnostic contact model consumed by [`crate::render`].

/// One contact (a person or entity), normalized so a single renderer
/// serves every contact-style provider. The provider's translate stage
/// builds these; the renderer never reaches back into provider rows.
#[derive(Debug, Clone)]
pub struct NormalizedContact {
    /// Stable UUID for this contact — used as the `markdown_uuid` and
    /// the grid-row `uuid`. The provider mints it (each owns its uuidv5
    /// namespace); for LinkedIn it's derived from the profile URL.
    pub contact_uuid: String,
    /// Stable UUID of the group this contact belongs to (a vCard
    /// addressbook, or LinkedIn's single "connections" group). Surfaces
    /// as the grid `conversation_uuid` so the UI can group members.
    pub group_uuid: String,
    /// Human label for the group — the grid `channel` /
    /// `conversation_name`, and (slugified) the on-disk directory name.
    pub group_label: String,
    /// Display name (`FN`, "First Last", …). Falls back to
    /// `external_id` then the uuid when absent.
    pub display_name: Option<String>,
    /// Upstream identifier surfaced as `external_id` and a frontmatter
    /// key — the vCard `UID`, or the LinkedIn profile URL.
    pub external_id: Option<String>,
    /// Source-side timestamp when one exists (vCard `REV:`, LinkedIn's
    /// "Connected On"). Passed through verbatim; never fabricated.
    pub when_ts: Option<String>,
    /// Canonical web URL for this contact, if any (the LinkedIn profile
    /// URL). Wired into the page Title's copy-link and the grid
    /// `source_url`.
    pub source_url: Option<String>,
    /// Ordered (label, value) detail rows. Rendered as a markdown table
    /// and folded into the grid row's search text, in this order.
    pub fields: Vec<ContactField>,
    /// Inline photo bytes, materialized into a sibling `blobs/` dir at
    /// render time. `None` for URL-only or photoless contacts.
    pub photo: Option<ContactPhoto>,
    /// URL-only photo (no bytes yet). Rendered as a "Photo URL" field.
    /// Fetching the bytes into blob_cas is a deferred enhancement (e.g.
    /// pulling a connection's picture off their LinkedIn page).
    pub photo_url: Option<String>,
}

/// A single labelled detail rendered in the contact's table.
#[derive(Debug, Clone)]
pub struct ContactField {
    pub label: String,
    pub value: String,
}

impl ContactField {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Decoded photo bytes plus a content-type guess. The renderer writes
/// these into a sibling `blobs/` directory.
#[derive(Debug, Clone)]
pub struct ContactPhoto {
    pub bytes: Vec<u8>,
    /// `image/jpeg`, `image/png`, …. Defaults to
    /// `application/octet-stream` when the source didn't say.
    pub content_type: String,
}
