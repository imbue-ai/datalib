//! A document written top to bottom as text, with the pieces that wrap a
//! row's `data-section-uuid` handed over keyed and the text between them
//! unkeyed: the [`Section`]s a page made this way is.

use datalib_etl_render::section::Section;

#[derive(Debug, Default)]
pub struct Pieces {
    done: Vec<Section>,
    open: String,
}

impl Pieces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_str(&mut self, s: &str) {
        self.open.push_str(s);
    }

    pub fn push(&mut self, c: char) {
        self.open.push(c);
    }

    pub fn keyed(&mut self, uuid: &str, md: String) {
        self.flush();
        self.done.push(Section::keyed(uuid, md));
    }

    pub fn into_sections(mut self) -> Vec<Section> {
        self.flush();
        self.done
    }

    fn flush(&mut self) {
        if !self.open.is_empty() {
            self.done
                .push(Section::unkeyed(std::mem::take(&mut self.open)));
        }
    }
}

impl std::fmt::Write for Pieces {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.open.push_str(s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::section::join;
    use std::fmt::Write as _;

    #[test]
    fn keyed_pieces_split_the_text_around_them() {
        let mut p = Pieces::new();
        p.push_str("head\n");
        p.keyed("a", "<div>a</div>\n".into());
        p.keyed("b", "<div>b</div>\n".into());
        let _ = writeln!(p, "tail");
        let sections = p.into_sections();
        let uuids: Vec<_> = sections.iter().map(|s| s.uuid.as_deref()).collect();
        assert_eq!(uuids, [None, Some("a"), Some("b"), None]);
        assert_eq!(join(&sections), "head\n<div>a</div>\n<div>b</div>\ntail\n");
        assert!(Pieces::new().into_sections().is_empty());
    }
}
