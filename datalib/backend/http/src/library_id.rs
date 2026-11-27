//! A library's id: the uuid in `system/library-id` that a copied
//! `datalib:` link carries, so the desktop app can tell which library a
//! link is for (`datalib/backend/uri/README.md` § "Which library").

use std::path::Path;

/// The library's id, minted now if it has none. Written whole or not at
/// all, so a reader never sees half of one.
pub fn ensure(root: &Path) -> anyhow::Result<String> {
    if let Some(id) = read(root)? {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let path = datalib_core::layout::library_id_file(root);
    datalib_runtime::atomic::write(&path, format!("{id}\n").as_bytes())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
    Ok(id)
}

/// The library's id, `None` when it has none yet. A file that holds
/// anything but one uuid is refused, naming it.
pub fn read(root: &Path) -> anyhow::Result<Option<String>> {
    let path = datalib_core::layout::library_id_file(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => anyhow::bail!("read {}: {e}", path.display()),
    };
    let id = text.trim();
    anyhow::ensure!(
        datalib_uuid_text::is_dashed(id),
        "{} should hold one uuid, and holds {id:?}",
        path.display()
    );
    Ok(Some(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_minted_once_and_kept() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("system")).unwrap();
        assert_eq!(read(root.path()).unwrap(), None);
        let first = ensure(root.path()).unwrap();
        assert!(datalib_uuid_text::is_dashed(&first), "{first}");
        assert_eq!(ensure(root.path()).unwrap(), first);
        assert_eq!(read(root.path()).unwrap(), Some(first));
    }

    #[test]
    fn a_file_that_is_not_a_uuid_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("system")).unwrap();
        std::fs::write(root.path().join("system/library-id"), "Default\n").unwrap();
        let err = ensure(root.path()).unwrap_err().to_string();
        assert!(err.contains("should hold one uuid"), "{err}");
    }
}
