use nostr::Event;
use serde_json::Value;

/// Source-faithful kind:0 metadata used by native identity components.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProfileMetadata {
    pub pubkey: String,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub banner: Option<String>,
    pub nip05: Option<String>,
    pub lud06: Option<String>,
    pub lud16: Option<String>,
}

pub fn decode_profile(event: &Event) -> ProfileMetadata {
    decode_profile_from_raw(&event.pubkey.to_hex(), &event.content)
}

pub fn decode_profile_from_raw(pubkey: &str, content: &str) -> ProfileMetadata {
    let object = serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    ProfileMetadata {
        pubkey: pubkey.to_string(),
        name: string_field(&object, &["name"]),
        display_name: string_field(&object, &["display_name", "displayName"]),
        about: string_field(&object, &["about"]),
        picture: string_field(&object, &["picture"]),
        banner: string_field(&object, &["banner"]),
        nip05: string_field(&object, &["nip05"]),
        lud06: string_field(&object, &["lud06"]),
        lud16: string_field(&object, &["lud16"]),
    }
}

fn string_field(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| object.get(*name)?.as_str())
        .find_map(nonempty)
}

fn nonempty(value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_standard_and_legacy_display_name_fields() {
        let profile = decode_profile_from_raw(
            "a",
            r#"{"name":"alice","display_name":" Alice Example ","picture":"https://example.com/a.jpg"}"#,
        );
        assert_eq!(profile.name.as_deref(), Some("alice"));
        assert_eq!(profile.display_name.as_deref(), Some("Alice Example"));
        assert_eq!(
            profile.picture.as_deref(),
            Some("https://example.com/a.jpg")
        );

        let legacy = decode_profile_from_raw("b", r#"{"displayName":"Legacy Name"}"#);
        assert_eq!(legacy.display_name.as_deref(), Some("Legacy Name"));
    }

    #[test]
    fn malformed_json_yields_an_immediately_renderable_empty_profile() {
        let profile = decode_profile_from_raw("a", "not json");
        assert_eq!(profile.pubkey, "a");
        assert_eq!(profile.name, None);
        assert_eq!(profile.picture, None);
    }
}
