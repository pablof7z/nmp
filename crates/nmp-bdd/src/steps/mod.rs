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

#[cfg(test)]
mod tests {
    use super::parse_people;

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
