//! Put a config file in the order data flows — each step below the steps
//! it reads, a group's entries together — and otherwise leave it as
//! written: an entry moves only when it has to, and the comments above it
//! move with it. The runner reads only `inputs`; the order is for the
//! person reading the file, and for the Sources screen, which lists
//! entries as the file does. `datalib-step topo-sort-config` runs it.
//!
//! The file is cut into entries as text, never re-serialized, and the
//! result is refused unless it parses to the same entries as the input.

use crate::config_lex::Kind;

/// The config text in data-flow order, or `None` when it already is.
/// `Err` when the text is not TOML, or when the reordered text would not
/// say the same thing — a bug here, reported rather than written.
pub fn sort_config(text: &str) -> Result<Option<String>, String> {
    let original: toml::Table = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let split = split(text);
    let entries = split
        .units
        .iter()
        .copied()
        .map(Entry::read)
        .collect::<Result<Vec<_>, _>>()?;
    let order = order(&entries);
    if order.iter().enumerate().all(|(i, &u)| i == u) {
        return Ok(None);
    }
    let pieces = std::iter::once(split.preamble)
        .chain(order.iter().map(|&u| split.units[u]))
        .chain(std::iter::once(split.postamble))
        .map(without_blank_edges)
        .filter(|p| !p.is_empty());
    let sorted = format!("{}\n", pieces.collect::<Vec<_>>().join("\n\n"));
    let reread: toml::Table = toml::from_str(&sorted)
        .map_err(|e| format!("the reordered file does not parse: {}", e.message()))?;
    if !same_entries(&original, &reread) {
        return Err("the reordered file would not hold the same entries; left as it is".into());
    }
    Ok(Some(sorted))
}

/// What a piece of the file is to the ordering.
#[derive(Debug, Default)]
struct Entry {
    /// A step's id, composed as the loader composes it.
    step: Option<String>,
    /// A `[[groups]]` entry's id, or the group a step or applet is filed
    /// under.
    group: Option<String>,
    is_group: bool,
    inputs: Vec<String>,
}

impl Entry {
    fn read(unit: &str) -> Result<Self, String> {
        let table: toml::Table = toml::from_str(unit).map_err(|e| e.message().to_string())?;
        let only = |key: &str| {
            table
                .get(key)
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_table())
        };
        let text =
            |t: &toml::Table, key: &str| t.get(key).and_then(|v| v.as_str()).map(str::to_string);
        if let Some(g) = only("groups") {
            return Ok(Entry {
                group: text(g, "id"),
                is_group: true,
                ..Entry::default()
            });
        }
        if let Some(s) = only("steps") {
            let group = text(s, "group");
            let step = match (&group, text(s, "function")) {
                (Some(g), Some(f)) => Some(format!("{g}/{f}")),
                _ => text(s, "id"),
            };
            let inputs = s
                .get("inputs")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Entry {
                step,
                group,
                inputs,
                ..Entry::default()
            });
        }
        if let Some(a) = only("applets") {
            return Ok(Entry {
                group: text(a, "group"),
                ..Entry::default()
            });
        }
        Ok(Entry::default())
    }
}

/// The new order, as indices into `entries`. Whole groups are ordered
/// first — a group, with every step and applet filed under it, goes after
/// the groups it reads — and then the entries inside each group. Where
/// that cannot be done (group A reads B, which reads A, though no step
/// reads itself) the entries are ordered one by one instead.
fn order(entries: &[Entry]) -> Vec<usize> {
    let deps = entry_deps(entries);
    let mut blocks: Vec<Vec<usize>> = Vec::new();
    let mut block_of = vec![0; entries.len()];
    let mut by_group = std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let b = match &e.group {
            Some(g) => *by_group.entry(g.as_str()).or_insert_with(|| {
                blocks.push(Vec::new());
                blocks.len() - 1
            }),
            None => {
                blocks.push(Vec::new());
                blocks.len() - 1
            }
        };
        blocks[b].push(i);
        block_of[i] = b;
    }
    let block_deps: Vec<Vec<usize>> = blocks
        .iter()
        .enumerate()
        .map(|(b, members)| {
            let mut d: Vec<usize> = members
                .iter()
                .flat_map(|&i| deps[i].iter().map(|&j| block_of[j]))
                .filter(|&x| x != b)
                .collect();
            d.sort_unstable();
            d.dedup();
            d
        })
        .collect();
    let (block_order, tangled) = stable_topo(&block_deps);
    if tangled {
        return stable_topo(&deps).0;
    }
    block_order
        .into_iter()
        .flat_map(|b| {
            let members = &blocks[b];
            let local: Vec<Vec<usize>> = members
                .iter()
                .map(|&i| {
                    deps[i]
                        .iter()
                        .filter_map(|j| members.iter().position(|m| m == j))
                        .collect()
                })
                .collect();
            stable_topo(&local)
                .0
                .into_iter()
                .map(|k| members[k])
                .collect::<Vec<_>>()
        })
        .collect()
}

/// For each entry, the entries that must come before it: the steps it
/// reads, and the `[[groups]]` entry it is filed under.
fn entry_deps(entries: &[Entry]) -> Vec<Vec<usize>> {
    let mut step_at = std::collections::HashMap::new();
    let mut group_at = std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(s) = &e.step {
            step_at.entry(s.as_str()).or_insert(i);
        }
        if let (true, Some(g)) = (e.is_group, &e.group) {
            group_at.entry(g.as_str()).or_insert(i);
        }
    }
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let reads = e.inputs.iter().filter_map(|id| step_at.get(id.as_str()));
            let filed = (!e.is_group)
                .then(|| e.group.as_deref().and_then(|g| group_at.get(g)))
                .flatten();
            let mut d: Vec<usize> = reads.chain(filed).copied().filter(|&j| j != i).collect();
            d.sort_unstable();
            d.dedup();
            d
        })
        .collect()
}

/// A topological order that keeps the given one wherever it can: each
/// time, the first item not yet placed whose dependencies all are. An
/// item moves down only past what it waits for, and never up. `true`
/// when a cycle had to be broken, which takes the first unplaced item.
fn stable_topo(deps: &[Vec<usize>]) -> (Vec<usize>, bool) {
    let mut placed = vec![false; deps.len()];
    let mut out = Vec::with_capacity(deps.len());
    let mut tangled = false;
    while out.len() < deps.len() {
        let ready = (0..deps.len()).find(|&i| !placed[i] && deps[i].iter().all(|&d| placed[d]));
        let next = ready.unwrap_or_else(|| {
            tangled = true;
            (0..deps.len())
                .find(|&i| !placed[i])
                .expect("an item is left")
        });
        placed[next] = true;
        out.push(next);
    }
    (out, tangled)
}

/// Every key but the three entry arrays unchanged, and those three
/// holding the same entries, in whatever order.
fn same_entries(a: &toml::Table, b: &toml::Table) -> bool {
    const MOVED: [&str; 3] = ["groups", "steps", "applets"];
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, va)| {
        let Some(vb) = b.get(k) else { return false };
        if !MOVED.contains(&k.as_str()) {
            return va == vb;
        }
        match (va.as_array(), vb.as_array()) {
            (Some(xa), Some(xb)) => {
                let mut left: Vec<&toml::Value> = xb.iter().collect();
                xa.len() == xb.len()
                    && xa.iter().all(|x| match left.iter().position(|y| *y == x) {
                        Some(p) => {
                            left.swap_remove(p);
                            true
                        }
                        None => false,
                    })
            }
            _ => va == vb,
        }
    })
}

/// The file cut into what comes before the first entry (top-level keys),
/// the entries, and the comments after the last one.
struct Split<'a> {
    preamble: &'a str,
    units: Vec<&'a str>,
    postamble: &'a str,
}

/// Cut the file at every table header that starts an entry, each entry
/// taking the comments and blank lines above its header. A `[steps.…]`
/// sub-table stays with its step; any other top-level table is an entry
/// of its own that nothing reads.
fn split(text: &str) -> Split<'_> {
    let lines = lines(text);
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.header.is_some_and(starts_entry))
        .map(|(i, _)| {
            let mut first = i;
            while first > 0 && lines[first - 1].trivia {
                first -= 1;
            }
            first
        })
        .collect();
    let Some(&first) = starts.first() else {
        return Split {
            preamble: text,
            units: Vec::new(),
            postamble: "",
        };
    };
    let tail = lines
        .iter()
        .rposition(|l| !l.trivia)
        .map_or(lines.len(), |i| i + 1);
    let at = |line: usize| lines.get(line).map_or(text.len(), |l| l.start);
    let units = starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let end = starts.get(k + 1).copied().unwrap_or(tail.max(s + 1));
            &text[at(s)..at(end)]
        })
        .collect();
    Split {
        preamble: &text[..at(first)],
        units,
        postamble: &text[at(tail.max(*starts.last().unwrap() + 1))..],
    }
}

/// A header that begins an entry: `[[groups]]`, `[[steps]]` and
/// `[[applets]]`, and any top-level table but their sub-tables.
fn starts_entry(name: &str) -> bool {
    const ENTRIES: [&str; 3] = ["groups", "steps", "applets"];
    match name.split_once('.') {
        Some((head, _)) => !ENTRIES.contains(&head.trim()),
        None => true,
    }
}

struct Line<'a> {
    start: usize,
    /// The table name, whitespace and brackets gone, when the line is a
    /// header.
    header: Option<&'a str>,
    /// Blank or a comment, and not inside a value.
    trivia: bool,
}

/// Each line, told apart from a `[` inside a multi-line string or array
/// by the tokens around it.
fn lines(text: &str) -> Vec<Line<'_>> {
    let tokens = crate::config_lex::tokens(text);
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut next = 0;
    let mut start = 0;
    while start < text.len() {
        // Only a multi-line string runs across a line start.
        let mut in_string = false;
        while let Some(t) = tokens.get(next).filter(|t| t.start < start) {
            if t.end > start {
                in_string = true;
                break;
            }
            if t.kind == Kind::Punct {
                match text.as_bytes()[t.start] {
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            next += 1;
        }
        let end = text[start..]
            .find('\n')
            .map_or(text.len(), |n| start + n + 1);
        let trimmed = text[start..end].trim();
        let in_value = in_string || depth > 0;
        let header = (!in_value && trimmed.starts_with('['))
            .then(|| header_name(trimmed))
            .flatten();
        out.push(Line {
            start,
            header,
            trivia: !in_value && (trimmed.is_empty() || trimmed.starts_with('#')),
        });
        start = end;
    }
    out
}

/// `[[ steps ]] # x` → `steps`; `[steps.params.api]` → `steps.params.api`.
fn header_name(line: &str) -> Option<&str> {
    let inner = line.trim_start_matches('[');
    let close = inner.find(']')?;
    Some(inner[..close].trim())
}

/// A piece with the blank lines at either end gone; comments stay.
fn without_blank_edges(piece: &str) -> &str {
    let from = piece
        .char_indices()
        .find(|&(_, c)| !c.is_whitespace())
        .map_or(piece.len(), |(i, _)| {
            piece[..i].rfind('\n').map_or(0, |n| n + 1)
        });
    piece[from..].trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(text: &str) -> Vec<String> {
        let w = crate::written::entries_as_written(text).unwrap();
        w.steps.into_iter().map(|s| s.id).collect()
    }

    fn groups(text: &str) -> Vec<String> {
        let w = crate::written::entries_as_written(text).unwrap();
        w.groups.into_iter().map(|g| g.id).collect()
    }

    /// Every input a step names that is in the file comes above it.
    fn flows_down(text: &str) -> bool {
        let w = crate::written::entries_as_written(text).unwrap();
        let at = |id: &str| w.steps.iter().position(|s| s.id == id);
        w.steps
            .iter()
            .enumerate()
            .all(|(i, s)| s.inputs.iter().all(|inp| at(inp).is_none_or(|j| j < i)))
    }

    const FLOWING: &str = r#"data_root = "~/datalib"

# ── slack ──
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
[steps.params.api]
channels = ["general"]

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

# ── the unified index ──
[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]
"#;

    /// A file already in order is left byte for byte, whatever its
    /// spacing.
    #[test]
    fn leaves_a_file_in_order_alone() {
        assert_eq!(sort_config(FLOWING).unwrap(), None);
        let spaced = FLOWING.replace("\n\n", "\n\n\n");
        assert_eq!(sort_config(&spaced).unwrap(), None);
    }

    /// The shape the wizard wrote before it kept the order, and the TNG
    /// fixture still writes: every source's qmd steps after the
    /// aggregator that reads them. Each goes back beside its source, the
    /// aggregator after them, and the comments stay with their entries.
    #[test]
    fn gathers_a_sources_steps_and_puts_the_index_after_them() {
        let text = r#"data_root = "~/datalib"

# ── slack ──
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
[steps.params.api]
channels = ["general"]

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

# ── notes ──
[[groups]]
id = "notes"
type = "slack"

[[steps]]
group = "notes"
function = "render_markdown"

# ── the unified index ──
[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "qmd_aggregator"
inputs = ["slack/keyword_index", "notes/keyword_index"]

# slack's search
[[steps]]
group = "slack"
function = "keyword_index"
inputs = ["slack/render_markdown"]

[[steps]]
group = "notes"
function = "keyword_index"
inputs = ["notes/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
"#;
        let sorted = sort_config(text).unwrap().expect("out of order");
        assert_eq!(
            ids(&sorted),
            [
                "slack/ingest",
                "slack/render_markdown",
                "slack/keyword_index",
                "notes/render_markdown",
                "notes/keyword_index",
                "unified_index/qmd_aggregator",
            ]
        );
        assert_eq!(groups(&sorted), ["slack", "notes", "unified_index"]);
        assert!(flows_down(&sorted));
        assert!(sorted.starts_with("data_root = \"~/datalib\"\n\n# ── slack ──\n"));
        // A comment travels with the entry under it, a sub-table with its step.
        let at = |s: &str| sorted.find(s).unwrap();
        assert!(at("# slack's search") < at("group = \"slack\"\nfunction = \"keyword_index\""));
        assert!(
            at("# slack's search")
                > at("function = \"render_markdown\"\ninputs = [\"slack/ingest\"]")
        );
        assert!(at("channels = [\"general\"]") < at("function = \"render_markdown\""));
        assert!(at("# ── the unified index ──") < at("function = \"qmd_aggregator\""));
        assert!(at("# ── notes ──") < at("# ── the unified index ──"));
        // Sorting again changes nothing.
        assert_eq!(sort_config(&sorted).unwrap(), None);
    }

    /// The scaffold wrote the index first and told people to add sources
    /// below it; the index goes to the end, with its banner.
    #[test]
    fn moves_an_index_written_first_to_the_end() {
        let text = r#"# ── the unified index ──
# The banner.

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"

# Sources go below.

# ── slack ──
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "render_markdown"
"#;
        let sorted = sort_config(text).unwrap().expect("out of order");
        assert_eq!(groups(&sorted), ["slack", "unified_index"]);
        assert!(sorted.starts_with("# Sources go below.\n\n# ── slack ──\n"));
        assert!(sorted.contains(
            "# ── the unified index ──\n# The banner.\n\n[[groups]]\nid = \"unified_index\""
        ));
        assert!(sorted
            .trim_end()
            .ends_with("command = \"datalib-applet unified_index\""));
    }

    /// A `[` at the start of a line inside a multi-line string or array
    /// is not a table header, and other top-level tables keep their
    /// place and their keys.
    #[test]
    fn reads_past_brackets_inside_values() {
        let text = r#"[run_history]
days = 30

[[steps]]
id = "export/csv"
command = """
[not a header]
"""
inputs = [
  "slack/render_markdown",
]

[[steps]]
id = "slack/render_markdown"
command = "render"
"#;
        let sorted = sort_config(text).unwrap().expect("out of order");
        assert_eq!(ids(&sorted), ["slack/render_markdown", "export/csv"]);
        assert!(sorted.starts_with("[run_history]\ndays = 30\n"));
        assert!(sorted.contains(
            "command = \"\"\"\n[not a header]\n\"\"\"\ninputs = [\n  \"slack/render_markdown\",\n]"
        ));
    }

    /// Group A reads B and B reads A, though no step reads itself: the
    /// groups cannot each stay whole, so the steps are ordered one by one.
    #[test]
    fn orders_steps_one_by_one_when_groups_read_each_other() {
        let text = r#"[[groups]]
id = "a"

[[steps]]
group = "a"
function = "two"
inputs = ["b/one"]

[[steps]]
group = "a"
function = "one"

[[groups]]
id = "b"

[[steps]]
group = "b"
function = "one"
inputs = ["a/one"]
"#;
        let sorted = sort_config(text).unwrap().expect("out of order");
        assert!(flows_down(&sorted));
        assert_eq!(ids(&sorted), ["a/one", "b/one", "a/two"]);
    }

    #[test]
    fn refuses_what_is_not_toml() {
        assert!(sort_config("[[steps").is_err());
    }
}
