//! The closed step-vocabulary catalog (approach doc §2.4). Extending it is a
//! reviewed change to one of these three files -- scenario `.feature` files
//! compose from the catalog, they never invent an ad-hoc step inline.

pub mod given;
pub mod then;
pub mod when;

/// Split a natural-language name list ("Alice, Bob, and Carol" / "Alice and
/// Bob" / "Alice") into its bare names -- the one bit of free-text parsing
/// every list-shaped step (`Given ... follows <people>`, `When I publish a
/// new follow list with <people>`, ...) shares, so scenario prose can read
/// exactly the way a product person would write a list.
pub fn parse_people(raw: &str) -> Vec<String> {
    raw.split(',')
        .flat_map(|part| part.split(" and "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The single ASCII letter a `"p"`/`"d"`/`"e"` step names. Its own helper so
/// every tag-shaped step rejects a non-letter the same way, with the same
/// message, instead of each one unwrapping an `Option` inline.
pub fn parse_tag(raw: &str) -> char {
    let mut chars = raw.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => c,
        _ => panic!("nmp-bdd: {raw:?} is not a single-letter tag name"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_people;
    use super::parse_tag;

    #[test]
    fn a_single_letter_is_a_tag_name() {
        assert_eq!(parse_tag("p"), 'p');
        assert_eq!(parse_tag("A"), 'A');
    }

    #[test]
    #[should_panic(expected = "is not a single-letter tag name")]
    fn a_multi_character_name_is_not_an_indexed_tag() {
        parse_tag("pp");
    }

    #[test]
    fn splits_a_three_person_oxford_comma_list() {
        assert_eq!(
            parse_people("Alice, Bob, and Carol"),
            vec!["Alice", "Bob", "Carol"]
        );
    }

    #[test]
    fn splits_a_two_person_list() {
        assert_eq!(parse_people("Alice and Bob"), vec!["Alice", "Bob"]);
    }

    #[test]
    fn a_single_name_is_itself() {
        assert_eq!(parse_people("Alice"), vec!["Alice"]);
    }
}
