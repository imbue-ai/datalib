//! Raw-store schema for the LinkedIn data-export provider — the
//! authoritative *manifest* of every file we try to ingest.
//!
//! Unlike the macro-driven `schema_raw.rs` of the API-backed providers,
//! LinkedIn's raw store is generic: one `(id, payload)` table per export
//! file, created lazily (see [`crate::extract`]). There are no row
//! structs to declare here. What lives here instead is the thing the
//! generic walker *can't* infer on its own:
//!
//!   * [`KNOWN_FILES`] — an enumeration of every file a complete
//!     LinkedIn export contains, each mapped to its canonical raw table
//!     name, natural-key column hint(s), whether it's message-shaped,
//!     and a one-line description. This is documentation first and a
//!     lookup table second.
//!   * [`canonical_table`] — slugify an export-relative path into a
//!     table name *and* strip the per-member numeric suffix LinkedIn
//!     bolts onto some filenames (`Comments_17529409.csv` →
//!     `comments`), so the same logical feed lands in the same table
//!     for every user.
//!   * The provider uuidv5 namespace ([`linkedin_ns`] / [`ns_id`]),
//!     shared by extract (row ids) and render (chat/message ids).
//!
//! ### Robustness contract
//!
//! The walker ingests *every* CSV it finds, listed here or not — a file
//! absent from [`KNOWN_FILES`] still gets a table (it just earns a WARN
//! so we notice new export shapes). And every file here is optional: a
//! user who deleted, never exported, or excluded a file (Thad omitted
//! `messages.csv` for privacy) just yields no table for it. So this
//! manifest is a description of the *maximal* export, never a
//! requirement.

use uuid::Uuid;

/// One row per file we expect in a complete LinkedIn export.
#[derive(Debug, Clone, Copy)]
pub struct KnownFile {
    /// Canonical raw table name (the [`canonical_table`] of the export
    /// path). Stable across members — no numeric suffix.
    pub table: &'static str,
    /// The file's name as LinkedIn ships it, relative to the export
    /// root. A trailing `_<memberid>` is shown as `_<id>` since it
    /// varies per user. For documentation / auditing only.
    pub export_name: &'static str,
    /// Natural-key column(s) for the row PK, in priority order. When all
    /// are empty for a row we fall back to a uuidv5 row hash. Empty =
    /// always hash (the common case — most LinkedIn CSVs have no id).
    pub id_cols: &'static [&'static str],
    /// True for the conversation feeds that share the `messages.csv`
    /// schema (`CONVERSATION ID, FROM, TO, DATE, CONTENT, …`); render
    /// turns each of these into chat markdown.
    pub message_shaped: bool,
    /// One-line description of what the file holds.
    pub note: &'static str,
}

/// Raw table that holds ingested `Articles/**/*.html` (the user's
/// published long-form posts). Not a CSV, so it's handled specially by
/// the walker, but enumerated in [`KNOWN_FILES`] like everything else.
pub const ARTICLES_TABLE: &str = "articles";

/// Every file a complete LinkedIn export can contain, as of the
/// 06-2026 "Complete" export format. Adding a row here is documentation;
/// the walker already ingests unlisted CSVs (with a WARN). Keep this
/// sorted by `table` for easy scanning.
pub const KNOWN_FILES: &[KnownFile] = &[
    KnownFile { table: "ad_targeting", export_name: "Ad_Targeting.csv", id_cols: &[], message_shaped: false, note: "Ad-targeting attributes LinkedIn inferred about you." },
    KnownFile { table: "ads_clicked", export_name: "Ads Clicked.csv", id_cols: &[], message_shaped: false, note: "Sponsored posts/ads you clicked, with timestamps." },
    KnownFile { table: "articles", export_name: "Articles/**/*.html", id_cols: &[], message_shaped: false, note: "Long-form articles you published (raw HTML, one row per file)." },
    KnownFile { table: "comments", export_name: "Comments_<id>.csv", id_cols: &["Link"], message_shaped: false, note: "Comments you left on posts (date, post link, message)." },
    KnownFile { table: "company_follows", export_name: "Company Follows.csv", id_cols: &["Organization"], message_shaped: false, note: "Companies/organizations you follow." },
    KnownFile { table: "connections", export_name: "Connections.csv", id_cols: &["URL"], message_shaped: false, note: "Your 1st-degree connections (name, profile URL, company, when connected). Has a Notes: preamble." },
    KnownFile { table: "education", export_name: "Education.csv", id_cols: &[], message_shaped: false, note: "Schools, degrees, and dates from your profile." },
    KnownFile { table: "email_addresses", export_name: "Email Addresses.csv", id_cols: &["Email Address"], message_shaped: false, note: "Email addresses on your account and their confirmed/primary flags." },
    KnownFile { table: "endorsement_given_info", export_name: "Endorsement_Given_Info.csv", id_cols: &[], message_shaped: false, note: "Skill endorsements you gave others." },
    KnownFile { table: "endorsement_received_info", export_name: "Endorsement_Received_Info.csv", id_cols: &[], message_shaped: false, note: "Skill endorsements others gave you." },
    KnownFile { table: "guide_messages", export_name: "guide_messages.csv", id_cols: &[], message_shaped: true, note: "LinkedIn 'guide' assistant conversation transcript." },
    KnownFile { table: "inferences_about_you", export_name: "Inferences_about_you.csv", id_cols: &[], message_shaped: false, note: "Attributes LinkedIn inferred about you (interests, seniority, etc.)." },
    KnownFile { table: "instantreposts", export_name: "InstantReposts_<id>.csv", id_cols: &["Link"], message_shaped: false, note: "Posts you reposted instantly (date, post link)." },
    KnownFile { table: "invitations", export_name: "Invitations.csv", id_cols: &["inviterProfileUrl", "inviteeProfileUrl", "Sent At"], message_shaped: false, note: "Connection invitations sent and received." },
    KnownFile { table: "job_applicant_saved_screening_question_responses", export_name: "Job Applicant Saved Screening Question Responses.csv", id_cols: &[], message_shaped: false, note: "Saved answers to job-application screening questions." },
    KnownFile { table: "jobs_job_applicant_saved_answers", export_name: "Jobs/Job Applicant Saved Answers.csv", id_cols: &[], message_shaped: false, note: "Saved standard answers used in job applications." },
    KnownFile { table: "jobs_job_applications", export_name: "Jobs/Job Applications.csv", id_cols: &["Job Url"], message_shaped: false, note: "Jobs you applied to via LinkedIn (company, title, date, Q&A)." },
    KnownFile { table: "jobs_job_seeker_preferences", export_name: "Jobs/Job Seeker Preferences.csv", id_cols: &[], message_shaped: false, note: "Your job-seeking preferences (locations, titles, open-to-work)." },
    KnownFile { table: "jobs_saved_jobs", export_name: "Jobs/Saved Jobs.csv", id_cols: &["Job Url"], message_shaped: false, note: "Jobs you saved." },
    KnownFile { table: "lan_ads_engagement", export_name: "LAN Ads Engagement.csv", id_cols: &[], message_shaped: false, note: "LinkedIn Audience Network ad-engagement events." },
    KnownFile { table: "languages", export_name: "Languages.csv", id_cols: &[], message_shaped: false, note: "Languages listed on your profile." },
    KnownFile { table: "learning", export_name: "Learning.csv", id_cols: &[], message_shaped: false, note: "LinkedIn Learning course activity (multi-line descriptions)." },
    KnownFile { table: "learning_coach_messages", export_name: "learning_coach_messages.csv", id_cols: &[], message_shaped: true, note: "LinkedIn Learning AI-coach conversation transcript (snake_case file)." },
    KnownFile { table: "learning_role_play_messages", export_name: "learning_role_play_messages.csv", id_cols: &[], message_shaped: true, note: "LinkedIn Learning role-play AI conversation transcript." },
    KnownFile { table: "learningcoachmessages", export_name: "LearningCoachMessages.csv", id_cols: &[], message_shaped: true, note: "LinkedIn Learning AI-coach transcript (CamelCase variant file)." },
    KnownFile { table: "logins", export_name: "Logins.csv", id_cols: &[], message_shaped: false, note: "Account login events (date, IP, user agent)." },
    KnownFile { table: "member_follows", export_name: "Member_Follows_<id>.csv", id_cols: &[], message_shaped: false, note: "Members you follow (date, status, name)." },
    KnownFile { table: "messages", export_name: "messages.csv", id_cols: &[], message_shaped: true, note: "Direct-message conversations with other members. The primary chat feed; rendered to markdown." },
    KnownFile { table: "patents", export_name: "Patents.csv", id_cols: &[], message_shaped: false, note: "Patents listed on your profile." },
    KnownFile { table: "phonenumbers", export_name: "PhoneNumbers.csv", id_cols: &[], message_shaped: false, note: "Phone numbers on your account." },
    KnownFile { table: "positions", export_name: "Positions.csv", id_cols: &[], message_shaped: false, note: "Work history / positions from your profile." },
    KnownFile { table: "profile", export_name: "Profile.csv", id_cols: &[], message_shaped: false, note: "Your core profile (name, headline, summary, industry, location)." },
    KnownFile { table: "profile_summary", export_name: "Profile Summary.csv", id_cols: &[], message_shaped: false, note: "Aggregate profile counters (connections, followers, etc.)." },
    KnownFile { table: "publications", export_name: "Publications.csv", id_cols: &[], message_shaped: false, note: "Publications listed on your profile." },
    KnownFile { table: "reactions", export_name: "Reactions_<id>.csv", id_cols: &["Link"], message_shaped: false, note: "Reactions (LIKE, EMPATHY, …) you gave posts." },
    KnownFile { table: "receipts_v2", export_name: "Receipts_v2.csv", id_cols: &["Invoice Number"], message_shaped: false, note: "Purchase receipts (subscriptions, etc.)." },
    KnownFile { table: "recommendations_given", export_name: "Recommendations_Given.csv", id_cols: &[], message_shaped: false, note: "Recommendations you wrote for others." },
    KnownFile { table: "recommendations_received", export_name: "Recommendations_Received.csv", id_cols: &[], message_shaped: false, note: "Recommendations others wrote for you." },
    KnownFile { table: "registration", export_name: "Registration.csv", id_cols: &[], message_shaped: false, note: "Account registration details (date, registered email/IP)." },
    KnownFile { table: "rich_media", export_name: "Rich_Media.csv", id_cols: &["Media Link"], message_shaped: false, note: "Rich-media items (images/links) attached to your content." },
    KnownFile { table: "saved_items", export_name: "Saved_Items_<id>.csv", id_cols: &["savedItem"], message_shaped: false, note: "Posts/items you saved for later." },
    KnownFile { table: "savedjobalerts", export_name: "SavedJobAlerts.csv", id_cols: &[], message_shaped: false, note: "Saved job-search alerts." },
    KnownFile { table: "searchqueries", export_name: "SearchQueries.csv", id_cols: &[], message_shaped: false, note: "Searches you ran on LinkedIn (time, query)." },
    KnownFile { table: "shares", export_name: "Shares_<id>.csv", id_cols: &["ShareLink"], message_shaped: false, note: "Posts you shared (date, link, commentary, visibility)." },
    KnownFile { table: "skills", export_name: "Skills.csv", id_cols: &["Name"], message_shaped: false, note: "Skills listed on your profile." },
    KnownFile { table: "verifications_verifications", export_name: "Verifications/Verifications.csv", id_cols: &[], message_shaped: false, note: "Identity / workplace verifications on your account." },
    KnownFile { table: "volunteering", export_name: "Volunteering.csv", id_cols: &[], message_shaped: false, note: "Volunteering experience from your profile." },
    KnownFile { table: "whatsapp_phone_numbers", export_name: "Whatsapp Phone Numbers.csv", id_cols: &[], message_shaped: false, note: "WhatsApp phone numbers linked to your account." },
];

/// Look up a file's manifest entry by its [`canonical_table`] name.
pub fn known_file(table: &str) -> Option<&'static KnownFile> {
    KNOWN_FILES.iter().find(|f| f.table == table)
}

/// Canonical table names of the message-shaped feeds, in manifest order.
/// Render walks these (any that exist + are non-empty) to produce chat
/// markdown.
pub fn message_tables() -> Vec<&'static str> {
    KNOWN_FILES
        .iter()
        .filter(|f| f.message_shaped)
        .map(|f| f.table)
        .collect()
}

/// Slugify an export-relative path into a SQL table name and strip the
/// per-member numeric suffix LinkedIn appends to some filenames.
///
/// Lowercase; every run of non-alphanumerics collapses to a single `_`;
/// a leading digit is prefixed with `t_`; and a trailing all-digits
/// segment (the member id in `Comments_17529409` → `comments`) is
/// dropped so the same logical feed maps to one table for every user.
/// `Receipts_v2` keeps its `v2` (not all digits).
pub fn canonical_table(rel: &str) -> String {
    // Drop a trailing file extension (`.csv`, `.html`) so it doesn't
    // slugify into a `_csv` suffix; callers may pass a full filename.
    let stem = std::path::Path::new(rel)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    let s = stem.to_lowercase();
    let mut out = String::new();
    let mut prev_us = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    let mut t = out.trim_matches('_').to_string();
    // Drop a trailing all-digits segment (the per-member id), but only
    // when something stable remains in front of it.
    if let Some((head, last)) = t.rsplit_once('_') {
        if !head.is_empty() && !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
            t = head.to_string();
        }
    }
    // Leading digit would be an awkward identifier; prefix it.
    if t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("t_{t}")
    } else {
        t
    }
}

/// Per-provider uuidv5 namespace for synthesized row / chat / message
/// ids. Shared by extract and render so the two agree.
pub fn linkedin_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"linkedin.frankweiler")
}

/// uuidv5 of `recipe` under the provider namespace, hyphenated.
pub fn ns_id(recipe: &str) -> String {
    Uuid::new_v5(&linkedin_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_strips_member_id_suffix() {
        assert_eq!(canonical_table("Comments_17529409.csv"), "comments");
        assert_eq!(
            canonical_table("Member_Follows_17529409.csv"),
            "member_follows"
        );
        assert_eq!(canonical_table("Saved_Items_17529409.csv"), "saved_items");
        // Not a member id — keep it.
        assert_eq!(canonical_table("Receipts_v2.csv"), "receipts_v2");
        // Paths + spaces.
        assert_eq!(canonical_table("Email Addresses.csv"), "email_addresses");
        assert_eq!(canonical_table("Jobs/Saved Jobs.csv"), "jobs_saved_jobs");
        // Leading digit guard.
        assert_eq!(canonical_table("123.csv"), "t_123");
    }

    #[test]
    fn known_tables_are_unique_and_self_consistent() {
        let mut seen = HashSet::new();
        for f in KNOWN_FILES {
            assert!(seen.insert(f.table), "duplicate table {}", f.table);
        }
        // The articles constant is enumerated.
        assert!(known_file(ARTICLES_TABLE).is_some());
        // Every message feed shares the messages table family.
        let msgs = message_tables();
        assert!(msgs.contains(&"messages"));
        assert!(msgs.contains(&"guide_messages"));
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn known_export_names_canonicalize_to_their_table() {
        // The canonical_table of a manifest entry's export name must
        // equal its declared table — except entries whose export_name is
        // a documentation placeholder rather than a literal filename
        // (`Articles/**/*.html`, `Comments_<id>.csv`).
        for f in KNOWN_FILES {
            if f.export_name.contains('*') || f.export_name.contains('<') {
                continue;
            }
            assert_eq!(
                canonical_table(f.export_name),
                f.table,
                "export_name {} canonicalizes wrong",
                f.export_name
            );
        }
    }

    #[test]
    fn namespace_is_stable() {
        assert_eq!(ns_id("chat:c1"), ns_id("chat:c1"));
        assert_ne!(ns_id("chat:c1"), ns_id("chat:c2"));
        assert_eq!(ns_id("x").len(), 36);
    }
}
