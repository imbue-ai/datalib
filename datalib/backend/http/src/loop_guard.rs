//! Catches a page feeding itself through the live stream: a request's
//! own effect — its log line, a walk it asked for — comes back as a
//! `root` frame, and the page refetches on the frame. Each hop is
//! counted, not prevented. The count rides the frame out (`chain`), comes
//! back on the refetch (`X-Datalib-Cause`) and is stored on the
//! request's log line, where the watcher reads it again. A chain that
//! reaches [`LOOP_AT`] is a loop, and the server says so in its log.

use datalib_runs::LogRow;

/// The request header a page echoes a frame's `chain` in, when the
/// fetch was made while that frame was being handled.
pub const CAUSE_HEADER: &str = "x-datalib-cause";

/// The tracing target of the warning; `target:http.loop` in the log
/// panel's search bar.
pub const TARGET: &str = "http.loop";

/// The field on a request line that stores its chain.
pub const CHAIN_FIELD: &str = "chain";

/// One refetch caused by its own echo is waste; five in a row is a
/// page that will not stop by itself.
pub const LOOP_AT: u32 = 5;

tokio::task_local! {
    static CHAIN: u32;
}

/// The hops behind a request: what its cause header says, 0 for a
/// request nothing on the stream caused.
pub fn request_chain(header: Option<&str>) -> u32 {
    header.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Warn at [`LOOP_AT`] and at every tenfold after it, so a loop left
/// running says so again without a line per hop.
pub fn warns_at(chain: u32) -> bool {
    let mut at = LOOP_AT;
    loop {
        if chain == at {
            return true;
        }
        if chain < at {
            return false;
        }
        match at.checked_mul(10) {
            Some(next) => at = next,
            None => return false,
        }
    }
}

/// The chain a frame continues when the server's own lines are what
/// moved: one past the longest chain among its request lines. `None`
/// when no request line is among them, so nothing a page did caused it.
/// Other server lines ride along without breaking the chain; the
/// warning itself is one of them.
pub fn burst_chain(lines: &[LogRow]) -> Option<u32> {
    lines
        .iter()
        .filter(|l| l.target.as_deref() == Some(crate::request_log::TARGET))
        .map(chain_of)
        .max()
        .map(|c| c.saturating_add(1))
}

fn chain_of(line: &LogRow) -> u32 {
    line.fields
        .as_deref()
        .and_then(|f| serde_json::from_str::<serde_json::Value>(f).ok())
        .and_then(|v| v.get(CHAIN_FIELD)?.as_u64())
        .map_or(0, |c| u32::try_from(c).unwrap_or(u32::MAX))
}

/// Run a request's handler knowing its chain, so a frame the handler
/// sends itself (`usage::sample_on_demand`) continues it.
pub async fn scope<F: std::future::Future>(chain: u32, f: F) -> F::Output {
    CHAIN.scope(chain, f).await
}

/// The chain a frame sent from inside a request continues; `None`
/// outside one.
pub fn from_this_request() -> Option<u32> {
    CHAIN.try_with(|c| c.saturating_add(1)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(target: &str, fields: Option<&str>) -> LogRow {
        LogRow {
            target: Some(target.into()),
            fields: fields.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn a_request_with_no_cause_starts_a_chain_at_zero() {
        assert_eq!(request_chain(None), 0);
        assert_eq!(request_chain(Some("3")), 3);
        assert_eq!(request_chain(Some("not a number")), 0);
    }

    #[test]
    fn a_burst_of_request_lines_continues_the_longest_chain() {
        let lines = [
            line(crate::request_log::TARGET, Some(r#"{"path":"/a"}"#)),
            line(
                crate::request_log::TARGET,
                Some(r#"{"path":"/b","chain":4}"#),
            ),
            line("ui.navigate", None),
        ];
        assert_eq!(burst_chain(&lines), Some(5));
    }

    #[test]
    fn a_burst_with_no_request_line_continues_nothing() {
        assert_eq!(burst_chain(&[]), None);
        assert_eq!(burst_chain(&[line("datalib_http::worker", None)]), None);
    }

    #[test]
    fn the_warning_comes_at_the_threshold_and_every_tenfold() {
        let warned: Vec<u32> = (0..6000).filter(|&c| warns_at(c)).collect();
        assert_eq!(warned, [5, 50, 500, 5000]);
        assert!(!warns_at(u32::MAX));
    }

    #[tokio::test]
    async fn a_frame_sent_inside_a_request_is_one_hop_past_it() {
        assert_eq!(from_this_request(), None);
        assert_eq!(scope(2, async { from_this_request() }).await, Some(3));
    }
}
