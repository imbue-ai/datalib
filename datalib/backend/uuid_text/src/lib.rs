//! The one place a uuid written as text is read. Sources and people write
//! the same uuid several ways: Notion's API with dashes and its links
//! without, Apple in capitals. Each is read here and written back one way,
//! lowercase with dashes, so nothing else in the tree spells a uuid out.

/// Hex digits in the groups 8-4-4-4-12, in either case.
pub fn is_dashed(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// `s` when it is a whole uuid, dashed or 32 bare hex digits, in either
/// case: lowercase with dashes.
pub fn canonical(s: &str) -> Option<String> {
    let hex = if is_dashed(s) {
        s.replace('-', "")
    } else if s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit()) {
        s.to_string()
    } else {
        return None;
    };
    let hex = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// The uuid a word ends in, in [`canonical`] form: a bare one, or the last
/// part of a slug or a link (`Shift-Roster-<id>`, `https://…/p/<id>?pvs=4`,
/// `…/chat/<id>/`).
pub fn at_end(word: &str) -> Option<String> {
    let word = word.trim();
    let word = word.split(['?', '#']).next().unwrap_or(word);
    let word = word.strip_suffix('/').unwrap_or(word);
    [36, 32].into_iter().find_map(|len| {
        let at = word.len().checked_sub(len)?;
        let cut = at.checked_sub(1).map(|b| word.as_bytes()[b]);
        if !matches!(cut, None | Some(b'-' | b'/')) {
            return None;
        }
        canonical(word.get(at..)?)
    })
}

/// Every way a source may store the uuid `s` is: dashed or not, lowercase
/// or capitals. What a lookup asks for, so a uuid matches however it was
/// typed and however its source stored it.
pub fn spellings(s: &str) -> Option<Vec<String>> {
    let dashed = canonical(s)?;
    let bare = dashed.replace('-', "");
    let mut all = vec![
        dashed.to_ascii_uppercase(),
        dashed,
        bare.to_ascii_uppercase(),
        bare,
    ];
    all.sort();
    all.dedup();
    Some(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "1701d000-abcd-4e00-a000-0000000000ef";

    #[test]
    fn a_uuid_reads_dashed_or_bare_in_either_case() {
        for typed in [
            ID,
            "1701D000-ABCD-4E00-A000-0000000000EF",
            "1701d000abcd4e00a0000000000000ef",
            "1701D000abcd4E00A0000000000000eF",
        ] {
            assert_eq!(canonical(typed).as_deref(), Some(ID), "{typed}");
        }
        for not in [
            "M-ncc1701-0042",
            "1701d000-abcd",
            "1701d000abcd4e00a0000000000000e",
            "x1701d000abcd4e00a0000000000000ef",
            "1701d000-abcd-4e00-a000-0000000000eg",
            "1701d000a-bcd-4e00-a000-0000000000ef",
            " 1701d000-abcd-4e00-a000-0000000000ef",
        ] {
            assert_eq!(canonical(not), None, "{not}");
        }
    }

    #[test]
    fn dashed_means_the_four_dashes_in_their_places() {
        assert!(is_dashed(ID));
        assert!(is_dashed("1701D000-ABCD-4E00-A000-0000000000EF"));
        assert!(!is_dashed("1701d000abcd4e00a0000000000000ef"));
        assert!(!is_dashed("1701d000a-bcd-4e00-a000-0000000000ef"));
    }

    #[test]
    fn a_uuid_is_found_at_the_end_of_a_slug_or_a_link() {
        for word in [
            ID,
            "1701d000abcd4e00a0000000000000ef",
            "Shift-Roster-1701d000abcd4e00a0000000000000ef",
            "picard-jean-luc-1701d000-abcd-4e00-a000-0000000000ef",
            "https://www.notion.so/ws/Away-Team-1701d000abcd4e00a0000000000000ef?pvs=4",
            "https://www.notion.so/1701d000abcd4e00a0000000000000ef#h1",
            "https://claude.ai/chat/1701d000-abcd-4e00-a000-0000000000ef/",
            " 1701D000ABCD4E00A0000000000000EF ",
        ] {
            assert_eq!(at_end(word).as_deref(), Some(ID), "{word}");
        }
        for word in [
            "plain-name",
            "short",
            "xxx1701d000-abcd-4e00-a000-0000000000ef",
            "deadbeef1701d000abcd4e00a0000000000000ef",
            "https://www.notion.so/ws/Away-Team",
        ] {
            assert_eq!(at_end(word), None, "{word}");
        }
    }

    /// A word whose last 36 bytes start inside a character must not
    /// panic on the slice.
    #[test]
    fn a_word_split_mid_character_is_no_uuid() {
        let word = format!("é{}", "a".repeat(35));
        assert_eq!(at_end(&word), None);
    }

    #[test]
    fn a_uuid_is_spelled_dashed_or_not_in_either_case() {
        let all = [
            "1701D000-ABCD-4E00-A000-0000000000EF",
            "1701D000ABCD4E00A0000000000000EF",
            "1701d000-abcd-4e00-a000-0000000000ef",
            "1701d000abcd4e00a0000000000000ef",
        ];
        for typed in all {
            assert_eq!(spellings(typed).unwrap(), all, "{typed}");
        }
        assert_eq!(
            spellings("00000001-1701-4000-8000-000000000001").unwrap(),
            [
                "00000001-1701-4000-8000-000000000001",
                "00000001170140008000000000000001"
            ],
            "digits alone have one case"
        );
        assert_eq!(spellings("M-ncc1701-0042"), None);
    }
}
