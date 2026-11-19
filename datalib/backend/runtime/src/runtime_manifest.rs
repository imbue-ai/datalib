//! The `runtime.manifest` a release tarball carries beside its binaries:
//! which assets of the same release to fetch on first use, each with its
//! sha256, size and URL. The tarball's own sha256 is what the installer verifies, so
//! the manifest is as pinned as the binaries, and the bytes it names are
//! refused unless they hash as it says.
//!
//! One asset per line, whitespace-separated, `#` starts a comment:
//!
//! ```text
//! cpu   runtime-x86_64-unknown-linux-gnu.tar.gz    <sha256> <bytes> <url>
//! cuda  ggml-cuda-x86_64-unknown-linux-gnu.tar.gz  <sha256> <bytes> <url>
//! ```
//!
//! `cpu` is always there; `cuda` only in the x86_64 gnu tarball.
//! `scripts/release/stage_tarball.sh` writes it; `scripts/stage_runtime.sh`
//! and `scripts/release/stage_cuda.sh` say what each asset holds. A shipped binary reads only the manifest its own release
//! put beside it.

use std::fmt;

/// File name of the manifest, beside the binaries.
pub const MANIFEST_FILE: &str = "runtime.manifest";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// Node plus the latchkey tree: the tree every Node tool runs from.
    Cpu,
    /// llama.cpp's CUDA backend, `lib/libggml-cuda.so`, which the
    /// embedder loads where there is an NVIDIA GPU
    /// (`datalib_runtime::cuda_module`).
    Cuda,
}

impl AssetKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "cpu" => Some(Self::Cpu),
            "cuda" => Some(Self::Cuda),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAsset {
    pub kind: AssetKind,
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub url: String,
}

impl RuntimeAsset {
    /// The directory a fetched asset is filed under: the first twelve hex
    /// digits of its sha256.
    pub fn dir_name(&self) -> &str {
        &self.sha256[..12]
    }

    pub fn megabytes(&self) -> u64 {
        self.bytes / 1_000_000
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    cpu: RuntimeAsset,
    cuda: Option<RuntimeAsset>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut cpu = None;
        let mut cuda = None;
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let asset = parse_line(line).map_err(|what| ParseError { line: i + 1, what })?;
            let slot = match asset.kind {
                AssetKind::Cpu => &mut cpu,
                AssetKind::Cuda => &mut cuda,
            };
            if slot.is_some() {
                return Err(ParseError {
                    line: i + 1,
                    what: format!("a second `{}` asset", asset.kind.as_str()),
                });
            }
            *slot = Some(asset);
        }
        let cpu = cpu.ok_or(ParseError {
            line: 0,
            what: "no `cpu` asset".to_string(),
        })?;
        Ok(Self { cpu, cuda })
    }

    pub fn cpu(&self) -> &RuntimeAsset {
        &self.cpu
    }

    pub fn cuda(&self) -> Option<&RuntimeAsset> {
        self.cuda.as_ref()
    }

    /// Render in the file's own format — what `stage_tarball.sh` writes
    /// and what a test writes to stand in for a release.
    pub fn render(&self) -> String {
        std::iter::once(&self.cpu)
            .chain(&self.cuda)
            .map(|asset| {
                format!(
                    "{} {} {} {} {}\n",
                    asset.kind.as_str(),
                    asset.name,
                    asset.sha256,
                    asset.bytes,
                    asset.url
                )
            })
            .collect()
    }

    pub fn new(cpu: RuntimeAsset) -> Self {
        Self { cpu, cuda: None }
    }

    pub fn with_cuda(self, cuda: RuntimeAsset) -> Self {
        Self {
            cuda: Some(cuda),
            ..self
        }
    }
}

fn parse_line(line: &str) -> Result<RuntimeAsset, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [kind, name, sha256, bytes, url] = fields[..] else {
        return Err(format!(
            "expected `<kind> <name> <sha256> <bytes> <url>`, got {} fields",
            fields.len()
        ));
    };
    let kind = AssetKind::parse(kind).ok_or_else(|| format!("unknown asset kind `{kind}`"))?;
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("`{sha256}` is not a sha256 hex digest"));
    }
    if name.contains('/') || name.starts_with('.') {
        return Err(format!("`{name}` is not a bare file name"));
    }
    let bytes: u64 = bytes
        .parse()
        .map_err(|_| format!("`{bytes}` is not a byte count"))?;
    if !fetchable(url) {
        return Err(format!(
            "`{url}`: only https URLs are fetched (or http from 127.0.0.1, for a test)"
        ));
    }
    Ok(RuntimeAsset {
        kind,
        name: name.to_string(),
        sha256: sha256.to_ascii_lowercase(),
        bytes,
        url: url.to_string(),
    })
}

/// `https`, or `http` to exactly `127.0.0.1` for a test's own server.
fn fetchable(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => url.host() == Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST)),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub what: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.what)
        } else {
            write!(f, "line {}: {}", self.line, self.what)
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn parses_the_cpu_asset() {
        let text = format!(
            "# written by release.yml\n\
             cpu runtime-x.tar.gz {SHA_A} 100 https://example.com/r.tar.gz # node\n\
             \n"
        );
        let m = Manifest::parse(&text).unwrap();
        assert_eq!(m.cpu().name, "runtime-x.tar.gz");
        assert_eq!(m.cpu().bytes, 100);
        assert_eq!(m.cpu().dir_name(), "aaaaaaaaaaaa");
        assert_eq!(m.cpu().url, "https://example.com/r.tar.gz");
        // Round-trips through its own renderer.
        assert_eq!(Manifest::parse(&m.render()).unwrap(), m);
    }

    #[test]
    fn parses_a_cuda_asset_beside_the_cpu_one() {
        let text = format!(
            "cpu runtime-x.tar.gz {SHA_A} 100 https://example.com/r.tar.gz\n\
             cuda ggml-cuda-x.tar.gz {SHA_B} 200 https://example.com/c.tar.gz\n"
        );
        let m = Manifest::parse(&text).unwrap();
        let cuda = m.cuda().unwrap();
        assert_eq!(cuda.kind, AssetKind::Cuda);
        assert_eq!(cuda.name, "ggml-cuda-x.tar.gz");
        assert_eq!(cuda.dir_name(), "bbbbbbbbbbbb");
        assert_eq!(Manifest::parse(&m.render()).unwrap(), m);
        let cpu_only = Manifest::new(m.cpu().clone()).render();
        assert_eq!(Manifest::parse(&cpu_only).unwrap().cuda(), None);
    }

    #[test]
    fn a_cuda_asset_alone_or_twice_is_refused() {
        let cuda = format!("cuda c.tar.gz {SHA_B} 1 https://x/\n");
        assert!(Manifest::parse(&cuda)
            .unwrap_err()
            .what
            .contains("no `cpu`"));
        let twice = format!("cpu a.tar.gz {SHA_A} 1 https://x/\n{cuda}{cuda}");
        let err = Manifest::parse(&twice).unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.what.contains("second `cuda`"));
    }

    #[test]
    fn refuses_what_it_cannot_pin() {
        let line = |s: &str| Manifest::parse(s).unwrap_err().what;
        assert!(line("").contains("no `cpu`"));
        assert!(line(&format!("gpu r.tar.gz {SHA_A} 1 https://x/")).contains("unknown asset kind"));
        assert!(line("cpu r.tar.gz abc 1 https://x/").contains("sha256"));
        assert!(line(&format!("cpu r.tar.gz {SHA_A} many https://x/")).contains("byte count"));
        assert!(line(&format!("cpu r.tar.gz {SHA_A} 1 http://x/")).contains("https"));
        for url in [
            "http://127.0.0.1.evil.example/r.tar.gz",
            "http://127.0.0.1@evil.example/r.tar.gz",
            "http://127.0.0.1:80@evil.example/r.tar.gz",
            "ftp://x/r.tar.gz",
        ] {
            assert!(
                line(&format!("cpu r.tar.gz {SHA_A} 1 {url}")).contains("https"),
                "{url}"
            );
        }
        Manifest::parse(&format!(
            "cpu r.tar.gz {SHA_A} 1 http://127.0.0.1:8080/r.tar.gz"
        ))
        .unwrap();
        assert!(line(&format!("cpu ../r.tar.gz {SHA_A} 1 https://x/")).contains("bare file name"));
        assert!(line(&format!("cpu r.tar.gz {SHA_A} 1")).contains("got 4 fields"));
        let twice = format!("cpu a.tar.gz {SHA_A} 1 https://x/\ncpu b.tar.gz {SHA_B} 1 https://x/");
        let err = Manifest::parse(&twice).unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.what.contains("second `cpu`"));
    }
}
