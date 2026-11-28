//! `datalib-step embed-prompts`: prompts on stdin, vectors on stdout,
//! through the embedder the `embed` step uses, so a vector made here is
//! the one the step would have made. What a rented GPU runs for a source
//! embedded elsewhere (docs/dev/embed_on_modal.md). One JSON object
//! a line each way, `{"hash", "prompt"}` in and `{"hash", "vector", "cut"}`
//! out, in order; stdout holds nothing else.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::search_index::EmbedPrompt;

/// Prompts read before they are embedded and their vectors written: the
/// caller sees results while the rest is still being read.
const GROUP: usize = 64;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub hash: String,
    pub prompt: String,
}

#[derive(Serialize)]
struct Embedded<'a> {
    hash: &'a str,
    vector: &'a [f32],
    cut: bool,
}

/// Line `n` (from 1) of the input; `None` for a blank one.
pub fn parse_line(n: usize, line: &str) -> Result<Option<Prompt>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .with_context(|| format!("line {n}: not a {{\"hash\", \"prompt\"}} object"))
}

/// Embeds every prompt `input` holds and writes each vector to `out`, a
/// group at a time; the count embedded. `open` runs only when the first
/// prompt arrives, so an empty input loads no model.
pub fn run<E: EmbedPrompt>(
    input: impl BufRead,
    mut out: impl Write,
    open: impl FnOnce() -> Result<E>,
) -> Result<usize> {
    let mut open = Some(open);
    let mut embedder: Option<E> = None;
    let mut group: Vec<Prompt> = Vec::with_capacity(GROUP);
    let mut done = 0;
    let mut lines = input.lines().enumerate();
    loop {
        let next = lines.next();
        if let Some((i, line)) = &next {
            let line = line
                .as_ref()
                .map_err(|e| anyhow::anyhow!("read stdin: {e}"))?;
            if let Some(p) = parse_line(i + 1, line)? {
                group.push(p);
            }
        }
        let at_end = next.is_none();
        if group.len() == GROUP || (at_end && !group.is_empty()) {
            let e = match &mut embedder {
                Some(e) => e,
                None => {
                    let Some(open) = open.take() else {
                        bail!("the embedder is gone");
                    };
                    embedder.insert(open()?)
                }
            };
            let prompts: Vec<String> = group.iter().map(|p| p.prompt.clone()).collect();
            let vectors = e.embed_fitted_many(&prompts)?;
            if vectors.len() != group.len() {
                bail!(
                    "the embedder gave {} vectors for {} prompts",
                    vectors.len(),
                    group.len()
                );
            }
            for (p, (vector, cut)) in group.iter().zip(&vectors) {
                let line = serde_json::to_string(&Embedded {
                    hash: &p.hash,
                    vector,
                    cut: *cut,
                })?;
                writeln!(out, "{line}").context("write stdout")?;
            }
            out.flush().context("write stdout")?;
            done += group.len();
            group.clear();
        }
        if at_end {
            return Ok(done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers each prompt with its length, counting the calls.
    struct Lengths(usize);

    impl EmbedPrompt for Lengths {
        fn embed_fitted_many(&mut self, prompts: &[String]) -> Result<Vec<(Vec<f32>, bool)>> {
            self.0 += 1;
            Ok(prompts
                .iter()
                .map(|p| (vec![p.len() as f32, 0.5], p.len() > 3))
                .collect())
        }
    }

    fn lines(n: usize) -> String {
        (0..n)
            .map(|i| {
                format!(
                    "{{\"hash\":\"h{i}\",\"prompt\":\"{}\"}}\n",
                    "x".repeat(i % 5)
                )
            })
            .collect()
    }

    #[test]
    fn each_prompt_comes_back_in_order_with_its_hash() {
        let mut out = Vec::new();
        let n = run(lines(150).as_bytes(), &mut out, || Ok(Lengths(0))).unwrap();
        assert_eq!(n, 150);
        let got: Vec<serde_json::Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(got.len(), 150);
        for (i, v) in got.iter().enumerate() {
            assert_eq!(v["hash"], format!("h{i}"));
            assert_eq!(v["vector"][0], (i % 5) as f64);
            assert_eq!(v["cut"], i % 5 > 3);
        }
    }

    #[test]
    fn an_empty_input_loads_no_model() {
        let mut out = Vec::new();
        let n = run("\n\n".as_bytes(), &mut out, || -> Result<Lengths> {
            panic!("opened the model for nothing")
        })
        .unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    /// Answers all but the last prompt of each call.
    struct OneShort;

    impl EmbedPrompt for OneShort {
        fn embed_fitted_many(&mut self, prompts: &[String]) -> Result<Vec<(Vec<f32>, bool)>> {
            Ok(prompts[1..].iter().map(|_| (vec![0.5], false)).collect())
        }
    }

    /// An embedder that answers fewer prompts than it was given fails the
    /// run; it is not written as fewer lines and an exit of 0.
    #[test]
    fn fewer_vectors_than_prompts_is_an_error() {
        let mut out = Vec::new();
        let err = run(lines(3).as_bytes(), &mut out, || Ok(OneShort)).unwrap_err();
        assert!(
            format!("{err:#}").contains("2 vectors for 3 prompts"),
            "{err:#}"
        );
        assert!(out.is_empty(), "nothing of a short group is written");
    }

    #[test]
    fn a_line_that_is_not_a_prompt_fails_by_its_number() {
        let input = format!("{}not json\n", lines(2));
        let err = run(input.as_bytes(), Vec::new(), || Ok(Lengths(0))).unwrap_err();
        assert!(format!("{err:#}").contains("line 3"), "{err:#}");
        let extra = r#"{"hash":"h","prompt":"p","model":"other"}"#;
        assert!(parse_line(1, extra).is_err(), "an unknown field is refused");
    }
}
