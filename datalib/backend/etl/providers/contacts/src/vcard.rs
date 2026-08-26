//! vCard stream splitting, shared by the download and render paths.
//!
//! Both sides receive a byte stream that may hold more than one vCard:
//! a CardDAV `<address-data>` response body, or a bulk `.vcf` export
//! dropped on disk by a render-only source. They must agree on where
//! one card ends and the next begins, so the split lives here rather
//! than in either path.

/// Split a vCard stream into one `String` per `BEGIN:VCARD` …
/// `END:VCARD` block.
///
/// Normalizes CRLF and bare CR to LF first (exports in the wild use
/// all three, so stay defensive).
///
/// Discards any text outside a block — wrapper-style exports
/// (CardDAV's `<address-data>` wrapping, leading mail-server
/// envelope text) wouldn't survive a round trip through this and
/// shouldn't.
pub(crate) fn split_vcards(body: &str) -> Vec<String> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VCARD") {
            current = Some(String::new());
        }
        if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
        if trimmed.eq_ignore_ascii_case("END:VCARD") {
            if let Some(buf) = current.take() {
                out.push(buf);
            }
        }
    }
    out
}
