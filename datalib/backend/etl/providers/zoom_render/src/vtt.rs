//! A Zoom transcript (WebVTT) as speaker turns.
//!
//! Zoom writes each cue's text as `Speaker Name: what they said` when it
//! knows who spoke, and the bare words when it does not. Consecutive
//! cues by one speaker are one turn, so the page reads as who said what
//! rather than as caption-sized fragments.

/// One cue of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    /// The cue's identifier line, or its position (from 1) without one.
    pub id: String,
    /// Milliseconds from the start of the recording.
    pub start_ms: i64,
    pub speaker: Option<String>,
    pub text: String,
}

/// Consecutive cues by one speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub first_cue: String,
    pub start_ms: i64,
    pub speaker: Option<String>,
    pub text: String,
}

/// A speaker prefix longer than this is a sentence with a colon in it.
const LONGEST_SPEAKER: usize = 80;

pub fn cues(vtt: &str) -> Vec<Cue> {
    let text = vtt.replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let lines: Vec<&str> = block.lines().filter(|l| !l.trim().is_empty()).collect();
        let Some(timing) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let Some(start_ms) = lines[timing]
            .split("-->")
            .next()
            .and_then(|t| timestamp_ms(t.trim()))
        else {
            continue;
        };
        let body = lines[timing + 1..].join("\n");
        if body.trim().is_empty() {
            continue;
        }
        let id = match timing {
            0 => (out.len() + 1).to_string(),
            _ => lines[timing - 1].trim().to_string(),
        };
        let (speaker, text) = split_speaker(&body);
        out.push(Cue {
            id,
            start_ms,
            speaker,
            text,
        });
    }
    out
}

pub fn turns(cues: &[Cue]) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for cue in cues {
        match out.last_mut() {
            Some(turn) if turn.speaker == cue.speaker => {
                turn.text.push(' ');
                turn.text.push_str(&cue.text);
            }
            _ => out.push(Turn {
                first_cue: cue.id.clone(),
                start_ms: cue.start_ms,
                speaker: cue.speaker.clone(),
                text: cue.text.clone(),
            }),
        }
    }
    out
}

fn split_speaker(body: &str) -> (Option<String>, String) {
    if let Some((who, said)) = body.split_once(": ") {
        let who = who.trim();
        if !who.is_empty() && who.len() <= LONGEST_SPEAKER && !who.contains('\n') {
            return (Some(who.to_string()), said.trim().to_string());
        }
    }
    (None, body.trim().to_string())
}

/// `HH:MM:SS.mmm` or `MM:SS.mmm`.
fn timestamp_ms(t: &str) -> Option<i64> {
    let (clock, millis) = t.split_once('.').unwrap_or((t, "0"));
    let mut secs: i64 = 0;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<i64>().ok()?;
    }
    let millis: i64 = format!("{millis:0<3}").get(..3)?.parse().ok()?;
    Some(secs * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VTT: &str =
        "WEBVTT\n\n1\n00:00:01.000 --> 00:00:04.500\nJean-Luc Picard: Number One, report.\n\n\
        2\n00:00:05.000 --> 00:00:09.000\nWilliam Riker: All decks report ready.\n\n\
        3\n00:00:09.500 --> 00:00:12.000\nWilliam Riker: Engineering wants an hour.\n\n\
        4\n01:00:17.250 --> 01:00:19.000\nMake it so.\n";

    #[test]
    fn cues_carry_their_speaker_and_their_time() {
        let got = cues(VTT);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].speaker.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(got[0].text, "Number One, report.");
        assert_eq!(got[0].start_ms, 1_000);
        assert_eq!(got[3].start_ms, 3_617_250);
        assert_eq!(got[3].speaker, None);
        assert_eq!(got[3].id, "4");
    }

    #[test]
    fn one_speakers_cues_in_a_row_are_one_turn() {
        let got = turns(&cues(VTT));
        let said: Vec<(Option<&str>, &str)> = got
            .iter()
            .map(|t| (t.speaker.as_deref(), t.text.as_str()))
            .collect();
        assert_eq!(
            said,
            [
                (Some("Jean-Luc Picard"), "Number One, report."),
                (
                    Some("William Riker"),
                    "All decks report ready. Engineering wants an hour."
                ),
                (None, "Make it so."),
            ]
        );
        assert_eq!(got[1].first_cue, "2");
    }

    #[test]
    fn a_file_with_windows_line_ends_and_no_cue_ids_still_reads() {
        let got = cues("WEBVTT\r\n\r\n00:00:02.5 --> 00:00:03.000\r\nData: Fascinating.\r\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "1");
        assert_eq!(got[0].start_ms, 2_500);
        assert_eq!(got[0].speaker.as_deref(), Some("Data"));
    }
}
