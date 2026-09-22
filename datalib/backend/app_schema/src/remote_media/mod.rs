// A rendered document's images on remote hosts are blocked until asked
// for (issue #648). The store `system/remote_media.doltlite_db` holds
// the asking and the answer: an allow row records a decision to load —
// one URL, everything in one document, everything on one host,
// everything from one source — and a media row records a URL fetched
// into the download CAS beside it, so opening the document again never
// reaches the host a second time. Both are committed per write, so
// `dolt log` is the history of who was let in. One table per file.

pub mod allow {
    include!("allow.rs");
}

pub mod media {
    include!("media.rs");
}

/// What an allow row covers. Its `key` is the thing named: the URL,
/// the document's `markdown_uuid`, the host, or the source's id.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AllowScope {
    Url,
    Document,
    Host,
    Source,
}

impl AllowScope {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// Every table of the store, for its DDL and its schema hash.
pub const DDL: &[(&str, &str)] = &[allow::DDL[0], media::DDL[0]];

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    /// The scope is bound as text and read back through `parse`, so
    /// the two spellings have to be one.
    #[test]
    fn allow_scope_strum_and_serde_agree() {
        for scope in AllowScope::VARIANTS {
            let json = serde_json::to_string(scope).unwrap();
            assert_eq!(json, format!("\"{}\"", scope.as_str()));
            assert_eq!(AllowScope::parse(scope.as_str()), Some(*scope));
        }
        assert_eq!(AllowScope::parse("everything"), None);
    }

    #[test]
    fn both_tables_present() {
        assert_eq!(DDL[0].0, "remote_media_allow");
        assert_eq!(DDL[1].0, "remote_media");
        let (_, cols) = allow::COLUMNS[0];
        assert!(cols.contains(&"scope") && cols.contains(&"key"));
        let (_, cols) = media::COLUMNS[0];
        assert!(cols.contains(&"sha256"));
    }
}
