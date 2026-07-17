//! Typed NIP-73 external-content targets (#572). Deliberately small: a
//! `PodcastEpisodeGuid` variant (this issue's required proof case) plus ONE
//! validated general variant carrying an already-canonicalized `(value,
//! kind)` pair -- NOT one variant per NIP-73 namespace (maintainer-decided
//! scope boundary, "General roots != enumerating every NIP-73 namespace as
//! its own variant"). A future proof case that needs its OWN typed
//! constructor validation gets its own variant then, not preemptively here.

/// A validated NIP-73 external-content target. `i_value`/`k_value` are the
/// canonical `I`/`K` tag payloads this target renders as -- private on
/// purpose (constructor-validated data only leaves through the accessors
/// [`Self::i_value`]/[`Self::k_value`], never a raw field a caller could
/// build with an unvalidated shortcut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip73Target {
    /// NIP-73's podcast episode GUID target: `I` is the raw GUID string,
    /// `K` is always the fixed literal [`Nip73Target::PODCAST_EPISODE_GUID_KIND`].
    PodcastEpisodeGuid(String),
    /// Any other NIP-73 external target: a caller-supplied, ALREADY
    /// canonicalized `(value, kind)` pair. This crate does not know how to
    /// canonicalize namespaces it doesn't own -- validation here is
    /// exactly "both cells are non-empty", never a namespace-specific
    /// format check.
    General { value: String, kind: String },
}

/// [`Nip73Target`] construction's typed refusal. Exhaustive; every variant
/// is constructed by a test (Reachability Gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip73TargetError {
    /// The `I` value was empty.
    EmptyValue,
    /// The `K` value was empty (general targets only -- the podcast
    /// variant's `K` is a fixed non-empty literal and can never trigger
    /// this).
    EmptyKind,
}

impl std::fmt::Display for Nip73TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyValue => f.write_str("NIP-73 target value must not be empty"),
            Self::EmptyKind => f.write_str("NIP-73 target kind must not be empty"),
        }
    }
}

impl std::error::Error for Nip73TargetError {}

impl Nip73Target {
    /// NIP-73's canonical `K` value for a podcast episode GUID.
    pub const PODCAST_EPISODE_GUID_KIND: &'static str = "podcast:item:guid";

    /// Construct a podcast-episode-GUID target. Refuses an empty GUID.
    pub fn podcast_episode_guid(guid: &str) -> Result<Self, Nip73TargetError> {
        if guid.is_empty() {
            return Err(Nip73TargetError::EmptyValue);
        }
        Ok(Self::PodcastEpisodeGuid(guid.to_string()))
    }

    /// Construct a general external target from an ALREADY-canonicalized
    /// `(value, kind)` pair. This crate does not own or validate any
    /// namespace's canonicalization rules beyond "non-empty" -- a caller
    /// composing e.g. an ISBN or URL target owns getting that value/kind
    /// pair canonical before calling this.
    pub fn general(value: &str, kind: &str) -> Result<Self, Nip73TargetError> {
        if value.is_empty() {
            return Err(Nip73TargetError::EmptyValue);
        }
        if kind.is_empty() {
            return Err(Nip73TargetError::EmptyKind);
        }
        Ok(Self::General {
            value: value.to_string(),
            kind: kind.to_string(),
        })
    }

    /// The canonical `I`/`i` tag payload.
    pub fn i_value(&self) -> &str {
        match self {
            Self::PodcastEpisodeGuid(guid) => guid,
            Self::General { value, .. } => value,
        }
    }

    /// The canonical `K`/`k` tag payload.
    pub fn k_value(&self) -> &str {
        match self {
            Self::PodcastEpisodeGuid(_) => Self::PODCAST_EPISODE_GUID_KIND,
            Self::General { kind, .. } => kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reachability Gate: every `Nip73TargetError` variant is constructed.
    #[test]
    fn podcast_episode_guid_refuses_empty_and_round_trips_i_and_k() {
        assert_eq!(
            Nip73Target::podcast_episode_guid(""),
            Err(Nip73TargetError::EmptyValue)
        );
        let target = Nip73Target::podcast_episode_guid("abc-123").unwrap();
        assert_eq!(target.i_value(), "abc-123");
        assert_eq!(target.k_value(), "podcast:item:guid");
    }

    #[test]
    fn general_target_refuses_either_empty_cell_and_round_trips() {
        assert_eq!(
            Nip73Target::general("", "isbn"),
            Err(Nip73TargetError::EmptyValue)
        );
        assert_eq!(
            Nip73Target::general("978-0", ""),
            Err(Nip73TargetError::EmptyKind)
        );
        let target = Nip73Target::general("978-0-13-468599-1", "isbn").unwrap();
        assert_eq!(target.i_value(), "978-0-13-468599-1");
        assert_eq!(target.k_value(), "isbn");
    }
}
