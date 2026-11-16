//! What an entry's category code (`Cat`) names. The codes are the web
//! page's own (`SCSTATUS_*` in its `scstatus-4.js`): a kind of entry is a
//! multiple of 50, and the codes just above it are its variants (401 to
//! 403 are a wet, a BM and a mixed diaper; 501 and 502 the start and the
//! end of a sleep).

/// The kind of entry `cat` is, as the web page groups it.
pub fn normalized(cat: i64) -> i64 {
    cat.div_euclid(50) * 50
}

/// The name a person knows the kind by; `None` for a code this build
/// does not know.
pub fn label(cat: i64) -> Option<&'static str> {
    Some(match normalized(cat) {
        100 => "Drop-off and pick-up",
        200 => "Solid food",
        300 => "Bottle",
        350 => "Nursing",
        400 => "Diaper",
        500 => "Sleep",
        600 => "Mood",
        700 => "Activity",
        800 => "Temperature",
        900 => "Location",
        1000 => "Photo",
        1100 => "Message",
        1200 => "Call",
        1300 => "Milestone",
        1400 => "Sickness",
        1500 => "Medicine",
        1600 => "Vaccine",
        1700 => "Weight",
        1800 => "Height",
        1900 => "Head size",
        2000 => "Doctor visit",
        2100 => "Question for the doctor",
        2200 => "Pumping",
        2250 => "Milk stash",
        2300 => "Medical",
        2400 => "Feed",
        2500 => "Potty",
        2600 => "Diary",
        2700 => "Cup",
        2800 => "Document",
        2900 => "Incident",
        3000 => "Need",
        3100 => "Behavior",
        3200 => "Assessment",
        3300 => "Lesson",
        3400 => "Observation",
        3500 => "Bath",
        3600 => "Teeth",
        3700 => "Transport",
        4000 => "Event",
        _ => return None,
    })
}

/// The label, or the code itself for one this build does not know.
pub fn name(cat: Option<i64>) -> String {
    match cat {
        Some(c) => label(c).map_or_else(|| format!("Entry ({c})"), String::from),
        None => "Entry".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variant_names_its_kind() {
        assert_eq!(label(350), Some("Nursing"));
        assert_eq!(label(354), Some("Nursing"));
        assert_eq!(label(402), Some("Diaper"));
        assert_eq!(label(502), Some("Sleep"));
        assert_eq!(label(2250), Some("Milk stash"));
        assert_eq!(label(2600), Some("Diary"));
        assert_eq!(name(Some(9999)), "Entry (9999)");
        assert_eq!(name(None), "Entry");
    }
}
