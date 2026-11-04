//! What a step's requests to the outside world come to, per host, for the
//! default view of the log. One request is a `debug` line, and a large
//! download is hundreds of thousands of them; this is the `info` line: the
//! first request to a host at once, what each minute added, and the whole
//! at the step's end (`docs/dev/logging.md` § "Where a line comes from").

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often a host still being talked to says so.
const EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Count {
    requests: u64,
    failed: u64,
    bytes: u64,
    wire: Duration,
}

impl Count {
    fn add(&mut self, attempt: &Attempt) {
        self.requests += 1;
        self.failed += u64::from(attempt.failed);
        self.bytes += attempt.bytes;
        self.wire += attempt.took;
    }
}

/// One request that left the machine, however it ended.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Attempt {
    pub failed: bool,
    pub bytes: u64,
    pub took: Duration,
}

#[derive(Debug)]
struct HostTally {
    since_said: Count,
    in_all: Count,
    said_at: Instant,
}

/// What one line says about one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Said {
    host: String,
    count: Count,
    /// Over the whole step, rather than since the last line.
    in_all: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Tally {
    hosts: BTreeMap<String, HostTally>,
}

impl Tally {
    /// Count `attempt`, and what to say now, if anything.
    pub(crate) fn note(&mut self, host: &str, attempt: Attempt, now: Instant) -> Option<Said> {
        let first = !self.hosts.contains_key(host);
        let tally = self.hosts.entry(host.to_string()).or_insert(HostTally {
            since_said: Count::default(),
            in_all: Count::default(),
            said_at: now,
        });
        tally.since_said.add(&attempt);
        tally.in_all.add(&attempt);
        if !first && now.duration_since(tally.said_at) < EVERY {
            return None;
        }
        tally.said_at = now;
        Some(Said {
            host: host.to_string(),
            count: std::mem::take(&mut tally.since_said),
            in_all: false,
        })
    }

    /// Each host's whole, leaving none. A host reached once has said all
    /// there is already.
    fn finish(&mut self) -> Vec<Said> {
        std::mem::take(&mut self.hosts)
            .into_iter()
            .filter(|(_, tally)| tally.in_all.requests > 1)
            .map(|(host, tally)| Said {
                host,
                count: tally.in_all,
                in_all: true,
            })
            .collect()
    }
}

/// The sentence for `said`, with the host first so a column of them reads
/// as a list of who was reached.
pub(crate) fn sentence(said: &Said) -> String {
    let Count {
        requests,
        failed,
        bytes,
        wire,
    } = said.count;
    let what = if requests == 1 { "request" } else { "requests" };
    let over = if said.in_all { " in all" } else { "" };
    format!(
        "outbound: {requests} {what} to {}{over}: {failed} failed, {:.1} MB, {:.1}s waiting on it",
        said.host,
        bytes as f64 / 1_000_000.0,
        wire.as_secs_f64(),
    )
}

/// The host of a URL as [`crate::http::ShownUrl`] prints one.
pub(crate) fn host_of(shown: &str) -> &str {
    let rest = shown.split_once("://").map_or(shown, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

static TALLY: Mutex<Option<Tally>> = Mutex::new(None);

fn say(said: &Said) {
    let count = &said.count;
    tracing::info!(
        target: datalib_log_filter::OUTBOUND_TARGET,
        host = %said.host,
        requests = count.requests,
        failed = count.failed,
        bytes = count.bytes,
        wire_ms = count.wire.as_millis() as u64,
        in_all = said.in_all,
        "{}",
        sentence(said),
    );
}

pub(crate) fn record(host: &str, attempt: Attempt) {
    let said = TALLY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Tally::default)
        .note(host, attempt, Instant::now());
    if let Some(said) = said {
        say(&said);
    }
}

/// The process's last word on who it reached: for a step's end.
pub fn say_totals() {
    let finished = TALLY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .map(|mut tally| tally.finish())
        .unwrap_or_default();
    for said in &finished {
        say(said);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(bytes: u64) -> Attempt {
        Attempt {
            failed: false,
            bytes,
            took: Duration::from_millis(100),
        }
    }

    /// The first request to a host is said at once, since "did it talk to
    /// Gmail" must be answerable while the step runs; then once a minute,
    /// counting what the minute added; then the whole.
    #[test]
    fn a_host_is_said_at_first_then_each_minute_then_in_all() {
        let t0 = Instant::now();
        let mut tally = Tally::default();
        let first = tally.note("mail.example", ok(10), t0).unwrap();
        assert_eq!(first.count.requests, 1);
        assert!(!first.in_all);

        assert_eq!(tally.note("mail.example", ok(10), t0 + EVERY / 2), None);
        let minute = tally
            .note(
                "mail.example",
                Attempt {
                    failed: true,
                    bytes: 0,
                    took: Duration::from_millis(100),
                },
                t0 + EVERY,
            )
            .unwrap();
        assert_eq!(minute.count.requests, 2, "the two since the first line");
        assert_eq!(minute.count.failed, 1);

        let totals = tally.finish();
        assert_eq!(totals.len(), 1);
        assert!(totals[0].in_all);
        assert_eq!(totals[0].count.requests, 3);
        assert_eq!(totals[0].count.bytes, 20);
    }

    #[test]
    fn hosts_are_counted_apart_and_one_reached_once_has_no_total() {
        let t0 = Instant::now();
        let mut tally = Tally::default();
        assert!(tally.note("a.example", ok(1), t0).is_some());
        assert!(tally.note("b.example", ok(1), t0).is_some());
        assert_eq!(tally.note("b.example", ok(1), t0), None);
        let totals = tally.finish();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].host, "b.example");
    }

    #[test]
    fn a_line_reads_as_leaving_the_machine() {
        let said = Said {
            host: "gmail.googleapis.com".into(),
            count: Count {
                requests: 412,
                failed: 2,
                bytes: 3_200_000,
                wire: Duration::from_millis(96_100),
            },
            in_all: true,
        };
        assert_eq!(
            sentence(&said),
            "outbound: 412 requests to gmail.googleapis.com in all: \
             2 failed, 3.2 MB, 96.1s waiting on it"
        );
    }

    #[test]
    fn the_host_is_read_from_a_shown_url() {
        assert_eq!(
            host_of("https://gmail.googleapis.com/gmail/v1/users/me"),
            "gmail.googleapis.com"
        );
        assert_eq!(host_of("https://example.com"), "example.com");
        assert_eq!(host_of("(not a URL)"), "(not a URL)");
    }
}
