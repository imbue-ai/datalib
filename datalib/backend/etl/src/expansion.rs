//! How far a compressed file may expand as it is read. Anyone can make an
//! export, and an archive can come out at a thousand times its size, or far
//! more when its entries share their bytes. What an archive says about its
//! own sizes may be false, so the bytes are counted as they come out of the
//! decompressor. `etl/files/README.md` § "An export as it was sent" has the
//! rules.

use std::fmt::Display;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};

/// Far past a real export: text compresses about 10 to 1, photos not at all.
pub const MAX_RATIO: u64 = 200;

/// What any archive may expand to, so that a small one of unusually
/// repetitive text is never refused.
pub const MIN_BYTES: u64 = 256 << 20;

/// A `.tgz` entry costs next to nothing compressed, and each one is a row
/// somewhere. A 50 GB Takeout part holds a few hundred thousand.
pub const MAX_ENTRIES: u64 = 5_000_000;

/// What is left of what one archive may expand to.
#[derive(Debug)]
pub struct Allowance {
    what: String,
    limit: u64,
    bytes_left: AtomicU64,
    entries_left: AtomicU64,
}

impl Allowance {
    pub fn new(limit: u64, entries: u64, what: impl Display) -> Self {
        Self {
            what: what.to_string(),
            limit,
            bytes_left: AtomicU64::new(limit),
            entries_left: AtomicU64::new(entries),
        }
    }

    /// For an archive of `len` bytes that `what` names to a person.
    pub fn for_archive(len: u64, what: impl Display) -> Self {
        let limit = len.saturating_mul(MAX_RATIO).max(MIN_BYTES);
        Self::new(limit, MAX_ENTRIES, what)
    }

    pub fn of_file(path: &Path) -> Result<Self> {
        let len = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        Ok(Self::for_archive(len, path.display()))
    }

    /// `reader`, failing the read that goes past what is left.
    pub fn meter<R: Read>(&self, reader: R) -> Metered<'_, R> {
        Metered {
            reader,
            allowance: self,
        }
    }

    /// Spend one entry of what is left.
    pub fn count_entry(&self) -> Result<()> {
        self.spend_entries(1)
    }

    /// Spend `n` entries at once, for an archive that lists them up front.
    pub fn spend_entries(&self, n: u64) -> Result<()> {
        let left = self.entries_left.load(Ordering::Relaxed);
        if n > left {
            bail!(
                "{} holds too many entries for a real archive, so it was refused; \
                 if you trust it, unpack it and point the source at the folder",
                self.what
            );
        }
        self.entries_left.store(left - n, Ordering::Relaxed);
        Ok(())
    }

    /// All of `reader`, one entry of the archive, in memory. `largest` is
    /// how much of it a reader can hold: a part this allowance would let
    /// through can still be more than a machine has.
    pub fn read_all(&self, reader: impl Read, largest: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.meter(reader)
            .take(largest.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > largest {
            bail!(
                "{} holds an entry larger than {largest} bytes, so it was refused; \
                 if you trust it, unpack it and point the source at the folder",
                self.what
            );
        }
        Ok(bytes)
    }

    fn refusal(&self) -> io::Error {
        io::Error::other(format!(
            "{} expands to more than {} bytes, too far for a real archive, so it was \
             refused; if you trust it, unpack it and point the source at the folder",
            self.what, self.limit
        ))
    }
}

/// A reader counted against an [`Allowance`].
pub struct Metered<'a, R> {
    reader: R,
    allowance: &'a Allowance,
}

impl<R: Read> Read for Metered<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(buf)?;
        let left = self.allowance.bytes_left.load(Ordering::Relaxed);
        if n as u64 > left {
            return Err(self.allowance.refusal());
        }
        self.allowance
            .bytes_left
            .store(left - n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(allowance: &Allowance, bytes: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        allowance.meter(bytes).read_to_end(&mut out)?;
        Ok(out)
    }

    /// What every entry of an archive reads comes out of one allowance, so
    /// entries that share their compressed bytes cannot each spend it anew.
    #[test]
    fn every_read_spends_the_one_allowance() {
        let a = Allowance::new(10, 1, "export.zip");
        assert_eq!(read(&a, &[1; 6]).unwrap(), [1; 6]);
        assert_eq!(read(&a, &[2; 4]).unwrap(), [2; 4]);
        let e = read(&a, &[3; 1]).unwrap_err().to_string();
        assert!(e.contains("export.zip"), "{e}");
        assert!(e.contains("more than 10 bytes"), "{e}");
    }

    #[test]
    fn exactly_the_allowance_is_allowed() {
        let a = Allowance::new(4, 1, "x");
        assert_eq!(read(&a, b"four").unwrap(), b"four");
        assert_eq!(read(&a, b"").unwrap(), b"");
    }

    #[test]
    fn an_archive_may_expand_by_the_ratio_but_never_less_than_the_floor() {
        assert_eq!(Allowance::for_archive(1, "x").limit, MIN_BYTES);
        let big = MIN_BYTES;
        assert_eq!(Allowance::for_archive(big, "x").limit, big * MAX_RATIO);
        assert_eq!(Allowance::for_archive(u64::MAX, "x").limit, u64::MAX);
    }

    #[test]
    fn an_entry_a_reader_cannot_hold_is_refused_though_the_archive_may_expand() {
        let a = Allowance::new(1 << 20, 1, "export.zip");
        assert_eq!(a.read_all(&[1u8; 8][..], 8).unwrap(), [1; 8]);
        let e = a.read_all(&[1u8; 9][..], 8).unwrap_err().to_string();
        assert!(e.contains("larger than 8 bytes"), "{e}");
    }

    #[test]
    fn entries_run_out() {
        let a = Allowance::new(1, 3, "export.tgz");
        a.spend_entries(2).unwrap();
        a.count_entry().unwrap();
        let e = a.count_entry().unwrap_err().to_string();
        assert!(e.contains("too many entries"), "{e}");
    }
}
