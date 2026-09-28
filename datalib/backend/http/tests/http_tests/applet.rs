//! Tests that start real applet processes and bind loopback ports, so
//! they cannot be sandboxed: `//datalib/backend/http:applet_tests` runs
//! this module alone, outside the sandbox.

mod applet_endpoint;
mod applet_proxy;

use std::path::{Path, PathBuf};

use datalib_schema::providers::Provider;

fn applet_command() -> String {
    let bin = PathBuf::from(std::env::var("APPLET_BIN").expect("APPLET_BIN set by the BUILD rule"))
        .canonicalize()
        .expect("applet binary exists");
    format!("{} slack", bin.display())
}

fn seed_doc(tree: &Path, md: &str, channel: &str, msgs: &[(i64, &str, &str, &str)]) {
    use datalib_etl_render::grid_index::RenderedMarkdown;
    use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
    use datalib_schema::grid_rows::GridRow;

    let mk = |uuid: String, index: Option<i64>, author: Option<&str>, text: &str, when: &str| {
        GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Slack)
            .kind(if index.is_some() {
                "Slack Message"
            } else {
                "Slack Thread"
            })
            .source_label("Slack")
            .is_document(index.is_none())
            .channel(Some(channel.to_string()))
            .created_at(Some(when.to_string()))
            .author(author.map(str::to_string))
            .message_index(index)
            .conversation_uuid(md)
            .entire_chat(format!("/chat/{md}"))
            .body(text)
            .markdown_uuid(Some(md.to_string()))
            .build()
            .unwrap()
    };

    let first = msgs.first().expect("a thread has at least one message");
    let mut rows = vec![mk(md.to_string(), None, None, first.2, first.3)];
    for (index, author, text, when) in msgs {
        rows.push(mk(
            format!("{md}-m{index}"),
            Some(*index),
            Some(author),
            text,
            when,
        ));
    }

    let store = IndexedMarkdownStore::open(tree).unwrap();
    store
        .put_document(
            tree,
            &RenderedMarkdown {
                markdown_uuid: md.to_string(),
                source_id: "slack".into(),
                upstream_cursor: None,
                bucket_key: None,
                md_path: tree.join(format!("{md}.md")),
                render_version: 1,
                rows,
                sections: Vec::new(),
                edges: Vec::new(),
                problems: Vec::new(),
            },
        )
        .unwrap();
    // The applet reads at HEAD, as the render step leaves it.
    store.commit("test").unwrap();
    store.close();
}
