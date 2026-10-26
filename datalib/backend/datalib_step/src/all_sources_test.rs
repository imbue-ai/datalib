//! `docs/user/config_examples/all_sources.toml` is generated from every
//! source's complete stanza (`source_docs`), so the one config that shows
//! every source cannot drift from the stanzas a test holds to the structs.
//! Regenerate with `bazel run //datalib/backend/datalib_step:all_sources.update`.

use strum::VariantArray;

use crate::methods::ingest_methods;
use crate::source_docs::{source_docs, with_group_id};
use crate::source_type::SourceType;
use datalib_source_common::ExampleKind;

const REL: &str = "docs/user/config_examples/all_sources.toml";

const HEAD: &str = r#"# Every source datalib can mirror, one group per way in, ready to copy.
#
# GENERATED: each group below is its source's complete example stanza,
# `datalib/backend/etl/providers/<type>_config/examples/complete_<method>.toml`,
# which a test loads and plans against the source's own config structs.
# Edit those, then regenerate this file:
#
#   bazel run //datalib/backend/datalib_step:all_sources.update
#
# `datalib-step examples <type>` prints the same stanzas and the minimal
# ones, and `datalib-step help <type> <method>` lists every option.
#
# This is a reference, not a turn-key config: running it would sync every
# source at once, and most need credentials (through latchkey), a backup
# pulled off a phone, or an export on disk. Copy the groups you want, and
# the index groups' inputs that name them.
#
# Shape: each source is a `[[groups]]` entry with a `type`, and steps under
# it, each declared as `group` + `function`. `ingest` brings the data into
# `<group>/ingest` (its `params` hold one method table, `[steps.params.api]`,
# `[steps.params.export]`, …, and the options every method shares);
# `render_markdown` turns that raw store into documents under
# `<group>/render_markdown`; `keyword_index` and `embed` put them in the
# search index; a source whose data is also numbers over time adds
# `render_time_series`. Neither writes a `command`: a step without one is
# the built-in `datalib-step`. A step's id is `<group>/<function>`, never
# written; it is what another step's `inputs` name.
#
# TOML shape: a table header ends the table it sits in, so within a step
# the plain keys (`group`, `function`, `inputs`) come first and the params
# headers last, and `data_root` comes before the first `[[…]]` header.
#
# A group's `id` is the folder under `data_root` its data goes in, chosen
# once; its `name` is free text the app shows and can change any time.
#
# Any `*_bytes` option takes an integer of bytes or a string with a unit:
# "5 MB", "512 KiB". KB/MB/GB are ×1000, KiB/MiB/GiB ×1024.
#
# Every value here is made up. Never commit a real token, key or address.

data_root = "~/datalib"
"#;

const DOCUMENT_INDEX: &str = "unified_document_index";
const TIME_SERIES_INDEX: &str = "unified_time_series_index";

struct Source {
    title: String,
    text: String,
    steps: Vec<(String, String)>,
}

fn sources() -> Vec<Source> {
    let mut out = Vec::new();
    for &t in SourceType::VARIANTS {
        let methods = ingest_methods(t);
        let docs = source_docs(t);
        for m in methods {
            let example = docs
                .example(ExampleKind::Complete, m.path)
                .unwrap_or_else(|| panic!("{t} has no complete example for `{}`", m.path));
            let id = if methods.len() == 1 {
                t.as_str().to_string()
            } else {
                format!("{t}-{}", m.path)
            };
            let text = with_group_id(example.toml, t.as_str(), &id);
            let steps = datalib_dag::config::check_text(&text)
                .cfg
                .steps
                .iter()
                .filter_map(|s| Some((s.id.clone(), s.function.clone()?)))
                .collect();
            out.push(Source {
                title: format!("{t} by `{}`", m.path),
                text,
                steps,
            });
        }
    }
    out
}

fn inputs(sources: &[Source], functions: &[&str]) -> String {
    let ids: Vec<String> = sources
        .iter()
        .flat_map(|s| &s.steps)
        .filter(|(_, f)| functions.contains(&f.as_str()))
        .map(|(id, _)| format!("  \"{id}\",\n"))
        .collect();
    format!("inputs = [\n{}]\n", ids.concat())
}

fn rule(title: &str) -> String {
    let lead = format!("# --- {title} ");
    format!(
        "{lead}{}\n",
        "-".repeat(74usize.saturating_sub(lead.chars().count()))
    )
}

fn generate() -> String {
    let sources = sources();
    let mut out = String::from(HEAD);
    out.push('\n');
    out.push_str(&rule("The indexes every source feeds"));
    out.push_str(&format!(
        "\n# The SQL index behind the grid and search reads every source's documents;\n\
         # the search index's aggregator reads every source's keyword index and\n\
         # embeddings, and keeps the search index to the sources it names.\n\
         [[groups]]\nid = \"{DOCUMENT_INDEX}\"\nname = \"Document Index\"\n\n\
         [[steps]]\ngroup = \"{DOCUMENT_INDEX}\"\nfunction = \"grid_index\"\n{}\n\
         [[steps]]\ngroup = \"{DOCUMENT_INDEX}\"\nfunction = \"qmd_aggregator\"\n{}\n\
         # The app's search, grid and document view.\n\
         [[applets]]\ngroup = \"{DOCUMENT_INDEX}\"\nid = \"{DOCUMENT_INDEX}\"\n\
         command = \"datalib-applet {DOCUMENT_INDEX}\"\n\n\
         # The time-series index reads every source's time series, without copying\n\
         # the points, for the charts.\n\
         [[groups]]\nid = \"{TIME_SERIES_INDEX}\"\nname = \"Time Series Index\"\n\n\
         [[steps]]\ngroup = \"{TIME_SERIES_INDEX}\"\nfunction = \"time_series_index\"\n{}\n\
         [[applets]]\ngroup = \"{TIME_SERIES_INDEX}\"\nid = \"{TIME_SERIES_INDEX}\"\n\
         command = \"datalib-applet {TIME_SERIES_INDEX}\"\n",
        inputs(&sources, &["render_markdown"]),
        inputs(&sources, &["keyword_index", "embed"]),
        inputs(&sources, &["render_time_series"]),
    ));
    for s in &sources {
        out.push('\n');
        out.push_str(&rule(&s.title));
        out.push('\n');
        out.push_str(s.text.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn all_sources_is_generated_from_the_stanzas() {
    let fresh = generate();
    if std::env::var("INSTA_UPDATE").as_deref() == Ok("always") {
        let root = std::env::var("INSTA_WORKSPACE_ROOT").unwrap_or_else(|_| ".".into());
        let path = std::path::Path::new(&root).join(REL);
        std::fs::write(&path, fresh).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        return;
    }
    let r = runfiles::Runfiles::create().expect("runfiles tree");
    let path = r
        .rlocation(format!("_main/{REL}"))
        .unwrap_or_else(|| panic!("rlocation for {REL}"));
    let on_disk =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        on_disk == fresh,
        "{REL} is out of date with the sources' complete stanzas; \
         run `bazel run //datalib/backend/datalib_step:all_sources.update`"
    );
}
