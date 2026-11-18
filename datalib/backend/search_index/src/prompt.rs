//! What the model is shown: qmd's prompts for embeddinggemma
//! (`formatQueryForEmbedding` and `formatDocForEmbedding` in its
//! `src/llm.ts`). A query and a document are framed differently, and a
//! vector is comparable only with one framed the way it was. A chunk's
//! vector is cached under the hash of exactly this string
//! (`chunk::chunk_hash`), so a change here misses the cache rather than
//! reading a vector made from another prompt.

pub fn query(text: &str) -> String {
    format!("task: search result | query: {text}")
}

pub fn document(text: &str, title: Option<&str>) -> String {
    let title = title.filter(|t| !t.is_empty()).unwrap_or("none");
    format!("title: {title} | text: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_without_a_title_says_none() {
        assert_eq!(document("hi", None), "title: none | text: hi");
        assert_eq!(document("hi", Some("")), "title: none | text: hi");
        assert_eq!(document("hi", Some("Plan")), "title: Plan | text: hi");
        assert_eq!(query("lunch"), "task: search result | query: lunch");
    }
}
