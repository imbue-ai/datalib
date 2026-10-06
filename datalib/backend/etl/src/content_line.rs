//! The content-line grammar iCalendar (RFC 5545 §3.1) and vCard (RFC 6350
//! §3.3) share: `[group.]NAME[;PARAM=V…]:value`, folded onto continuation
//! lines, with `BEGIN:`/`END:` nesting components. Calendar and contacts
//! both read through here; what a property *means* stays with them. It
//! does not validate.

/// One content line, unfolded. The value is as written, escapes and all;
/// [`Property::text`] and [`Property::text_list`] undo the TEXT escapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// The `item1` of vCard's `item1.EMAIL`, which ties a property to the
    /// `X-ABLabel` beside it.
    pub group: Option<String>,
    /// Uppercased.
    pub name: String,
    /// Keys uppercased, values unquoted. A bare parameter (vCard 2.1's
    /// `TEL;WORK:`) is a `TYPE`.
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Property {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// The value with TEXT escapes undone.
    pub fn text(&self) -> String {
        unescape_text(&self.value)
    }

    /// A structured or list value — vCard's `ORG` and `ADR` split on `;`,
    /// `CATEGORIES` on `,` — cut only where `sep` is not escaped, each
    /// part then unescaped.
    pub fn text_list(&self, sep: char) -> Vec<String> {
        split_unescaped(&self.value, sep)
            .into_iter()
            .map(unescape_text)
            .collect()
    }
}

/// `BEGIN:NAME` … `END:NAME`, with its properties and the components
/// nested inside it, both in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub props: Vec<Property>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.props
            .iter()
            .filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    /// A property's TEXT value, `None` when absent or blank.
    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name)
            .map(Property::text)
            .filter(|s| !s.trim().is_empty())
    }
}

/// Every top-level component in `text`. A line outside any component,
/// and an `END` that closes nothing, are skipped: exports in the wild
/// carry both.
pub fn parse(text: &str) -> Vec<Component> {
    let mut top: Vec<Component> = Vec::new();
    let mut stack: Vec<Component> = Vec::new();
    for line in unfold(text) {
        let Some(prop) = parse_line(&line) else {
            continue;
        };
        if prop.name == "BEGIN" {
            stack.push(Component {
                name: prop.value.trim().to_ascii_uppercase(),
                ..Default::default()
            });
        } else if prop.name == "END" {
            let name = prop.value.trim().to_ascii_uppercase();
            if !stack.iter().any(|c| c.name == name) {
                continue;
            }
            // Close anything left open inside it: a missing END is a
            // truncated component, not a reason to lose its parent.
            while let Some(done) = stack.pop() {
                let closes = done.name == name;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None => top.push(done),
                }
                if closes {
                    break;
                }
            }
        } else if let Some(open) = stack.last_mut() {
            open.props.push(prop);
        }
    }
    while let Some(done) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(done),
            None => top.push(done),
        }
    }
    top
}

/// Content lines with folding undone: a line that starts with a space or
/// tab continues the one before it, minus that one character. Any line
/// ending is accepted.
pub fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match (line.chars().next(), out.last_mut()) {
            (Some(' ' | '\t'), Some(prev)) => prev.push_str(&line[1..]),
            _ if line.is_empty() => {}
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// One unfolded line, or `None` when it has no `:`. A parameter value
/// may be quoted, and a quoted one may hold `;`, `:` and `,`.
pub fn parse_line(line: &str) -> Option<Property> {
    let mut name_end = None;
    let mut value_start = None;
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes && name_end.is_none() => name_end = Some(i),
            ':' if !in_quotes => {
                value_start = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = value_start?;
    let name_end = name_end.unwrap_or(colon);
    let head = line[..name_end].trim();
    let (group, name) = match head.rsplit_once('.') {
        Some((group, name)) => (Some(group.to_string()), name),
        None => (None, head),
    };
    let name = name.to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = if name_end < colon {
        split_params(&line[name_end + 1..colon])
    } else {
        Vec::new()
    };
    Some(Property {
        group,
        name,
        params,
        value: line[colon + 1..].to_string(),
    })
}

fn split_params(s: &str) -> Vec<(String, String)> {
    let mut chunk = String::new();
    let mut in_quotes = false;
    let mut chunks = Vec::new();
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                chunk.push(c);
            }
            ';' if !in_quotes => chunks.push(std::mem::take(&mut chunk)),
            _ => chunk.push(c),
        }
    }
    chunks.push(chunk);
    let unquote = |v: &str| {
        let v = v.trim();
        v.strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v)
            .to_string()
    };
    chunks
        .iter()
        .filter(|chunk| !chunk.trim().is_empty())
        .map(|chunk| match chunk.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_uppercase(), unquote(v)),
            None => ("TYPE".to_string(), unquote(chunk)),
        })
        .collect()
}

/// `s` cut at every `sep` no backslash escapes. The parts keep their
/// escapes, for [`unescape_text`].
pub fn split_unescaped(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            _ if c == sep => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// TEXT escapes undone (RFC 5545 §3.3.11, RFC 6350 §3.4): `\n` and `\N`
/// are a line break, and a backslash before anything else is that thing.
pub fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfolds_and_parses_quoted_params() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ncc-1701-d\r\nSUMMARY:Senior staff\r\n  meeting\r\nATTENDEE;CN=\"Riker, William\";PARTSTAT=ACCEPTED:mailto:riker@enterprise.test\r\nDESCRIPTION:Agenda:\\nSaucer separation\\, drill\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let cal = parse(text);
        assert_eq!(cal.len(), 1);
        let ev = cal[0].children_named("VEVENT").next().unwrap();
        assert_eq!(ev.text("SUMMARY").as_deref(), Some("Senior staff meeting"));
        let att = ev.prop("ATTENDEE").unwrap();
        assert_eq!(att.param("cn"), Some("Riker, William"));
        assert_eq!(att.value, "mailto:riker@enterprise.test");
        assert_eq!(
            ev.text("DESCRIPTION").as_deref(),
            Some("Agenda:\nSaucer separation, drill")
        );
    }

    #[test]
    fn a_missing_end_does_not_lose_the_parent() {
        let cal = parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:a\nBEGIN:VALARM\nACTION:DISPLAY\nEND:VEVENT\nEND:VCALENDAR\n");
        let ev = cal[0].children_named("VEVENT").next().unwrap();
        assert_eq!(ev.text("UID").as_deref(), Some("a"));
        assert_eq!(ev.children_named("VALARM").count(), 1);
    }

    /// A colon inside a quoted parameter is not where the value starts:
    /// splitting at the first `:` read Picard's label as his address.
    #[test]
    fn a_vcard_line_keeps_its_group_and_its_quoted_colon() {
        let p = parse_line(
            "item1.EMAIL;TYPE=WORK;X-LABEL=\"Bridge: day shift\";PREF:jlpicard@enterprise.test",
        )
        .unwrap();
        assert_eq!(p.group.as_deref(), Some("item1"));
        assert_eq!(p.name, "EMAIL");
        assert_eq!(p.value, "jlpicard@enterprise.test");
        assert_eq!(p.param("x-label"), Some("Bridge: day shift"));
        assert_eq!(
            p.params
                .iter()
                .filter(|(k, _)| k == "TYPE")
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>(),
            ["WORK", "PREF"]
        );
    }

    #[test]
    fn a_structured_value_splits_only_where_the_separator_is_not_escaped() {
        let org =
            parse_line(r"ORG:Starfleet\; Command;USS Enterprise\, NCC-1701-D;Bridge").unwrap();
        assert_eq!(
            org.text_list(';'),
            ["Starfleet; Command", "USS Enterprise, NCC-1701-D", "Bridge"]
        );
        let adr = parse_line(r"ADR;TYPE=WORK:;;Ready Room\nDeck 1;;;;").unwrap();
        assert_eq!(
            adr.text_list(';'),
            ["", "", "Ready Room\nDeck 1", "", "", "", ""]
        );
        assert_eq!(split_unescaped(r"a\\;b", ';'), [r"a\\", "b"]);
        let note = parse_line(r"NOTE:Make it so\, Number One.\nEngage\\").unwrap();
        assert_eq!(note.text(), "Make it so, Number One.\nEngage\\");
    }
}
