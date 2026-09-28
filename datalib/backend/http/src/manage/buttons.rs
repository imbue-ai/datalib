//! What a Manage row's Sync button and on/off switch say: whether Sync
//! can be pressed and what it would do, and which way the switch sits.
//! Pure functions over what the row builder already knows.

use datalib_columns::Action;
use datalib_dag::supervisor::tick::StateKind;

/// What pressing Sync would do (`Ok`, the hover), or why it cannot be
/// pressed (`Err`, the disabled hover).
pub type SyncOffer = Result<String, String>;

const DOWNSTREAM: &str = "then rebuild everything downstream";

/// A source step always runs when synced. Any other step runs only if it
/// is out of date, on what its inputs already hold, so Sync on one the
/// loop has recorded as up to date would do nothing and is disabled.
/// `feeding` are the source steps upstream of it.
pub fn step_sync(is_source: bool, state: Option<StateKind>, feeding: &[String]) -> SyncOffer {
    if is_source {
        return Ok(format!("Run this step now, {DOWNSTREAM} of it."));
    }
    let sources = match feeding {
        [] => "its sources".to_string(),
        [one] => one.clone(),
        many => many.join(", "),
    };
    match state {
        Some(StateKind::Idle) => Err(format!(
            "Up to date: nothing it reads, and neither its code nor its settings, has changed \
             since it last succeeded. Sync {sources} to fetch what\u{2019}s new, or reset this \
             step to rebuild it anyway."
        )),
        Some(StateKind::Stale) => Ok(format!(
            "Out of date. Rerun it on what its inputs already hold, {DOWNSTREAM}. Nothing \
             upstream runs; sync {sources} to fetch what\u{2019}s new."
        )),
        _ => Ok(format!(
            "Rerun it on what its inputs already hold if it is out of date, {DOWNSTREAM}. \
             Nothing upstream runs; sync {sources} to fetch what\u{2019}s new."
        )),
    }
}

/// A group with a source step syncs from it. One without (the index)
/// syncs its own steps the way `step_sync` syncs one: those out of date
/// rerun, so Sync is disabled when the loop has recorded all of them as
/// up to date. `states` are the group's steps', dropped ones left out.
pub fn group_sync(has_source: bool, states: &[Option<StateKind>]) -> SyncOffer {
    if has_source {
        return Ok(format!("Sync now: fetch what\u{2019}s new, {DOWNSTREAM}."));
    }
    if states.is_empty() {
        return Err("Nothing under this group runs.".to_string());
    }
    if states.iter().all(|s| *s == Some(StateKind::Idle)) {
        return Err(
            "Up to date: nothing its steps read, and neither their code nor their settings, \
             has changed since they last succeeded. Sync a source to fetch what\u{2019}s new."
                .to_string(),
        );
    }
    Ok(format!(
        "Rerun this group\u{2019}s out-of-date steps on what their inputs already hold, \
         {DOWNSTREAM}. Nothing upstream runs."
    ))
}

/// The on/off switch: whether the loop may start what the row stands
/// for. `off` of `total` steps are turned off, by `by`. A group reads
/// on unless every step under it is off; turning it off turns off the
/// rest, and turning it on turns them all on.
pub fn switch(
    group: bool,
    off: usize,
    total: usize,
    by: Option<&str>,
    blocked: Option<String>,
) -> Action {
    let on = total == 0 || off < total;
    // `ui` is whoever is looking at the screen.
    let who = match by {
        None => "someone",
        Some("ui") => "you",
        Some(by) => by,
    };
    let hint = match (on, group) {
        (false, false) => format!(
            "Off: {who} turned it off, so every sync skips it. Turn on to include it \
             again; that alone starts nothing."
        ),
        (false, true) => format!(
            "Off: {who} turned off its steps, so every sync skips them. Turn on to \
             include them again; that alone starts nothing."
        ),
        (true, false) => "On: it runs in syncs. Turn off to skip it until turned back on; \
                          if it is running, it stops."
            .to_string(),
        (true, true) if off == 0 => "On: its steps run in syncs. Turn off to skip them until \
                                     turned back on; any that are running stop."
            .to_string(),
        (true, true) => format!(
            "Partly on: {off} of {total} steps are off. Turn off to skip the rest too; any \
             that are running stop."
        ),
    };
    Action {
        id: "in_syncs".into(),
        label: "Runs in syncs".into(),
        enabled: blocked.is_none(),
        hint: Some(hint),
        disabled_reason: blocked,
        danger: false,
        on: Some(on),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeding() -> Vec<String> {
        vec!["mail/ingest".to_string()]
    }

    /// A derived step used to be unsyncable outright; a render whose
    /// code moved needs a way to rerun without a fresh download.
    #[test]
    fn a_derived_step_syncs_unless_the_loop_found_it_up_to_date() {
        assert!(step_sync(false, Some(StateKind::Stale), &feeding())
            .unwrap()
            .starts_with("Out of date."));
        assert!(step_sync(false, None, &feeding()).is_ok());
        assert!(step_sync(false, Some(StateKind::Failed), &feeding()).is_ok());
        let idle = step_sync(false, Some(StateKind::Idle), &feeding()).unwrap_err();
        assert!(idle.contains("Sync mail/ingest"), "{idle}");
    }

    #[test]
    fn a_source_step_always_syncs() {
        assert!(step_sync(true, Some(StateKind::Idle), &[]).is_ok());
    }

    #[test]
    fn a_group_without_a_source_syncs_while_any_step_is_not_up_to_date() {
        let idle = Some(StateKind::Idle);
        assert!(group_sync(false, &[idle, idle]).is_err());
        assert!(group_sync(false, &[idle, Some(StateKind::Stale)]).is_ok());
        assert!(group_sync(false, &[]).is_err());
        assert!(group_sync(true, &[idle]).is_ok());
    }

    #[test]
    fn a_group_switch_reads_on_until_every_step_is_off() {
        assert_eq!(switch(true, 0, 2, None, None).on, Some(true));
        let partly = switch(true, 1, 2, Some("ui"), None);
        assert_eq!(partly.on, Some(true));
        assert!(partly.hint.unwrap().starts_with("Partly on"));
        let off = switch(true, 2, 2, Some("claude"), None);
        assert_eq!(off.on, Some(false));
        assert!(off.hint.unwrap().contains("claude turned off"));
    }

    #[test]
    fn a_switch_the_ui_turned_off_says_you_did() {
        let off = switch(false, 1, 1, Some("ui"), None);
        assert!(off.hint.unwrap().starts_with("Off: you turned it off"));
    }

    #[test]
    fn a_blocked_switch_is_disabled_with_the_reason() {
        let a = switch(false, 0, 1, None, Some("Not in the pipeline".into()));
        assert!(!a.enabled);
        assert_eq!(a.disabled_reason.as_deref(), Some("Not in the pipeline"));
    }
}
