//! Config schema for the `calendar` source: which calendars to mirror
//! and how to reach them. Schema-only, so the step runner can name
//! `CalendarConfig` without linking the downloader.

use datalib_source_common::{LatchkeySettings, LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// Fastmail's CalDAV root. Its bare host answers 404 to a PROPFIND, so
/// discovery has to start here (or at `/.well-known/caldav`).
pub const FASTMAIL_CALDAV_URL: &str = "https://caldav.fastmail.com/dav/";

/// The ingest step's params for a `calendar` source. Exactly one of the
/// four method tables is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors, forwarded whole to
    /// the download client.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// Google Calendar, through Google's API (latchkey's
    /// `google-calendar` service).
    #[serde(default)]
    pub google: Option<CalendarSelection>,
    /// Fastmail, over CalDAV at [`FASTMAIL_CALDAV_URL`] (latchkey's
    /// `fastmail-dav` service, which holds an app password).
    #[serde(default)]
    pub fastmail: Option<CalendarSelection>,
    /// Any other CalDAV server: iCloud, Nextcloud, Radicale, ….
    #[serde(default)]
    pub caldav: Option<CaldavSync>,
    /// A directory of `.ics` files: a Google Takeout `Calendar/` folder,
    /// an export from Apple Calendar.
    #[serde(default)]
    pub ics: Option<LocalPath>,
}

/// Which download path a source selected.
#[derive(Debug, Clone)]
pub enum CalendarMethod<'a> {
    Google {
        calendars: &'a [String],
        window: Window<'a>,
    },
    /// CalDAV, with the server it starts discovery from.
    Caldav {
        server_url: &'a str,
        calendars: &'a [String],
        window: Window<'a>,
    },
    Ics(&'a LocalPath),
}

/// The stretch of days a source mirrors, `YYYY-MM-DD`, both ends
/// included; either end may be open. An event is in it when any of it
/// falls inside — a series is, when an occurrence does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Window<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
}

impl Window<'_> {
    pub fn is_open(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    fn validate(&self, table: &str) -> anyhow::Result<()> {
        for (key, v) in [("since", self.since), ("until", self.until)] {
            if let Some(v) = v {
                if !is_yyyy_mm_dd(v) {
                    anyhow::bail!("`{table}.{key}` must be a date, YYYY-MM-DD; got {v:?}");
                }
            }
        }
        if let (Some(s), Some(u)) = (self.since, self.until) {
            if s > u {
                anyhow::bail!("`{table}.until` ({u}) is before `{table}.since` ({s})");
            }
        }
        Ok(())
    }
}

fn is_yyyy_mm_dd(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

impl CalendarConfig {
    /// The one method this source holds, or why there is not exactly one.
    pub fn method(&self) -> anyhow::Result<CalendarMethod<'_>> {
        let mut held: Vec<(&str, CalendarMethod<'_>)> = Vec::new();
        if let Some(s) = &self.google {
            held.push((
                "google",
                CalendarMethod::Google {
                    calendars: &s.calendars,
                    window: s.window(),
                },
            ));
        }
        if let Some(s) = &self.fastmail {
            held.push((
                "fastmail",
                CalendarMethod::Caldav {
                    server_url: FASTMAIL_CALDAV_URL,
                    calendars: &s.calendars,
                    window: s.window(),
                },
            ));
        }
        if let Some(c) = &self.caldav {
            held.push((
                "caldav",
                CalendarMethod::Caldav {
                    server_url: &c.server_url,
                    calendars: &c.calendars,
                    window: Window {
                        since: c.since.as_deref(),
                        until: c.until.as_deref(),
                    },
                },
            ));
        }
        if let Some(p) = &self.ics {
            held.push(("ics", CalendarMethod::Ics(p)));
        }
        match held.len() {
            1 => Ok(held.pop().expect("len checked").1),
            0 => anyhow::bail!(
                "a calendar source names none of `google`, `fastmail`, `caldav` or `ics`"
            ),
            _ => anyhow::bail!(
                "a calendar source sets more than one of {} — pick one. To mirror the same \
                 calendars two ways, declare two sources.",
                held.iter()
                    .map(|(name, _)| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        let (table, window) = match self.method()? {
            CalendarMethod::Caldav {
                server_url, window, ..
            } => {
                if !(server_url.starts_with("https://") || server_url.starts_with("http://")) {
                    anyhow::bail!("`caldav.server_url` must be an http(s) URL, got {server_url:?}");
                }
                let table = if self.fastmail.is_some() {
                    "fastmail"
                } else {
                    "caldav"
                };
                (table, window)
            }
            CalendarMethod::Google { window, .. } => ("google", window),
            CalendarMethod::Ics(_) => return Ok(()),
        };
        window.validate(table)
    }
}

/// Which of an account's calendars to mirror.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarSelection {
    /// Calendar names as the service shows them, or their ids. Empty
    /// mirrors every calendar the account can see. Each calendar keeps
    /// its own sync position, so adding one here later downloads it
    /// whole on the next run.
    #[serde(default)]
    pub calendars: Vec<String>,
    /// The first day to mirror, `YYYY-MM-DD`. With `until`, a window:
    /// only events with some part inside it are mirrored, and only the
    /// changed occurrences of a series that fall inside it. A windowed
    /// calendar is listed whole every run rather than resumed.
    #[serde(default)]
    pub since: Option<String>,
    /// The last day to mirror, `YYYY-MM-DD`, included.
    #[serde(default)]
    pub until: Option<String>,
}

impl CalendarSelection {
    fn window(&self) -> Window<'_> {
        Window {
            since: self.since.as_deref(),
            until: self.until.as_deref(),
        }
    }
}

/// The `caldav` table: a server, plus the same selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaldavSync {
    /// Where discovery starts. The server's root is usually enough:
    /// discovery follows `/.well-known/caldav` when the root does not
    /// answer. Examples: `https://caldav.icloud.com/`,
    /// `https://cloud.example.com/remote.php/dav/`.
    pub server_url: String,
    /// As [`CalendarSelection::calendars`].
    #[serde(default)]
    pub calendars: Vec<String>,
    /// As [`CalendarSelection::since`].
    #[serde(default)]
    pub since: Option<String>,
    /// As [`CalendarSelection::until`].
    #[serde(default)]
    pub until: Option<String>,
}

pub type CalendarRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for CalendarConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("google"),
        datalib_source_common::IngestMethod::origin("fastmail"),
        datalib_source_common::IngestMethod::origin("caldav"),
        datalib_source_common::IngestMethod::local("ics"),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: serde_json::Value) -> CalendarConfig {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn fastmail_is_caldav_at_the_dav_root() {
        let cfg = parse(serde_json::json!({"fastmail": {"calendars": ["Work"]}}));
        match cfg.method().unwrap() {
            CalendarMethod::Caldav {
                server_url,
                calendars,
                ..
            } => {
                assert_eq!(server_url, FASTMAIL_CALDAV_URL);
                assert_eq!(calendars, ["Work"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn caldav_carries_its_selection_beside_the_url() {
        let cfg = parse(serde_json::json!({
            "caldav": {"server_url": "https://caldav.icloud.com/", "calendars": ["Home"]}
        }));
        cfg.validate().unwrap();
        let CalendarMethod::Caldav { calendars, .. } = cfg.method().unwrap() else {
            panic!("caldav");
        };
        assert_eq!(calendars, ["Home"]);
    }

    #[test]
    fn a_window_is_two_dates_in_order() {
        let google = |since: &str, until: &str| {
            parse(serde_json::json!({"google": {"since": since, "until": until}}))
        };
        let c = google("2025-11-01", "2025-11-30");
        c.validate().unwrap();
        let CalendarMethod::Google { window, .. } = c.method().unwrap() else {
            panic!("google");
        };
        assert_eq!(window.since, Some("2025-11-01"));
        assert!(!window.is_open());
        assert!(google("2025-11-30", "2025-11-01").validate().is_err());
        assert!(google("Nov 1", "2025-11-30").validate().is_err());
        let open = parse(serde_json::json!({"fastmail": {"since": "2025-11-01"}}));
        open.validate().unwrap();
    }

    #[test]
    fn exactly_one_method() {
        let none = parse(serde_json::json!({}));
        assert!(none.method().is_err());
        let two = parse(serde_json::json!({"google": {}, "ics": {"path": "/x"}}));
        let err = two.method().unwrap_err().to_string();
        assert!(err.contains("`google`") && err.contains("`ics`"), "{err}");
    }

    #[test]
    fn a_misspelled_key_is_refused() {
        let r: Result<CalendarConfig, _> =
            serde_json::from_value(serde_json::json!({"google": {"calendar": ["x"]}}));
        assert!(r.is_err());
    }
}
