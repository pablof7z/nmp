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
    /// NIP-73's podcast episode GUID target: the stored `String` is the
    /// BARE GUID (this variant's ergonomic constructor input); the wire
    /// `I`/`i` value [`Self::i_value`] renders is the full
    /// `podcast:item:guid:<guid>` string NIP-73's own table (and NIP-22's
    /// own podcast example) require -- `K` is always the fixed literal
    /// [`Nip73Target::PODCAST_EPISODE_GUID_KIND`].
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
    /// A `K`/`k` cell of [`Nip73Target::PODCAST_EPISODE_GUID_KIND`]
    /// declared an `I`/`i` value that did NOT carry the required
    /// `podcast:item:guid:` prefix -- a decode-time-only refusal (never
    /// reachable through the ergonomic constructors, which always render
    /// the prefix themselves).
    MissingPodcastGuidPrefix,
}

impl std::fmt::Display for Nip73TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyValue => f.write_str("NIP-73 target value must not be empty"),
            Self::EmptyKind => f.write_str("NIP-73 target kind must not be empty"),
            Self::MissingPodcastGuidPrefix => f.write_str(
                "podcast-episode-guid I/i value is missing its podcast:item:guid: prefix",
            ),
        }
    }
}

impl std::error::Error for Nip73TargetError {}

impl Nip73Target {
    /// NIP-73's canonical `K` value for a podcast episode GUID.
    pub const PODCAST_EPISODE_GUID_KIND: &'static str = "podcast:item:guid";

    /// NIP-73's canonical `I`/`i` value PREFIX for a podcast episode GUID --
    /// the wire value is `podcast:item:guid:<guid>`, NEVER the bare GUID
    /// (NIP-73's own table, and NIP-22's own podcast example, both use the
    /// prefixed form; a bare GUID is non-conformant and silently splits an
    /// episode's thread from conformant clients that only ever look for the
    /// prefixed value).
    const PODCAST_EPISODE_GUID_I_PREFIX: &'static str = "podcast:item:guid:";

    /// Construct a podcast-episode-GUID target from its bare GUID (never
    /// the prefixed wire value -- see [`Self::i_value`]). Refuses an empty
    /// GUID.
    pub fn podcast_episode_guid(guid: &str) -> Result<Self, Nip73TargetError> {
        if guid.is_empty() {
            return Err(Nip73TargetError::EmptyValue);
        }
        Ok(Self::PodcastEpisodeGuid(guid.to_string()))
    }

    /// Parse an already-decoded `I`/`i` value that a `K`/`k` cell of
    /// [`Self::PODCAST_EPISODE_GUID_KIND`] declares -- the wire value MUST
    /// carry the [`Self::PODCAST_EPISODE_GUID_I_PREFIX`] prefix; a value
    /// that doesn't (e.g. a bare GUID some other composer wrote) is a typed
    /// refusal, never silently reinterpreted.
    pub(crate) fn parse_podcast_episode_guid_i_value(
        i_value: &str,
    ) -> Result<Self, Nip73TargetError> {
        let guid = i_value
            .strip_prefix(Self::PODCAST_EPISODE_GUID_I_PREFIX)
            .ok_or(Nip73TargetError::MissingPodcastGuidPrefix)?;
        Self::podcast_episode_guid(guid)
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

    /// The canonical `I`/`i` tag payload. For [`Self::PodcastEpisodeGuid`]
    /// this is NIP-73's full `podcast:item:guid:<guid>` wire value -- NOT
    /// the bare GUID [`Self::podcast_episode_guid`] takes as input.
    pub fn i_value(&self) -> String {
        match self {
            Self::PodcastEpisodeGuid(guid) => {
                format!("{}{guid}", Self::PODCAST_EPISODE_GUID_I_PREFIX)
            }
            Self::General { value, .. } => value.clone(),
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
        // NIP-73's wire `I`/`i` value is the FULL prefixed string, never
        // the bare GUID the ergonomic constructor takes.
        assert_eq!(target.i_value(), "podcast:item:guid:abc-123");
        assert_eq!(target.k_value(), "podcast:item:guid");
    }

    /// `parse_podcast_episode_guid_i_value` round-trips a conformant wire
    /// value and refuses one missing the required prefix (the decode-time
    /// door `Nip73TargetError::MissingPodcastGuidPrefix` exists for).
    #[test]
    fn parse_podcast_episode_guid_i_value_requires_the_prefix() {
        let target =
            Nip73Target::parse_podcast_episode_guid_i_value("podcast:item:guid:abc-123").unwrap();
        assert_eq!(
            target,
            Nip73Target::podcast_episode_guid("abc-123").unwrap()
        );
        assert_eq!(
            Nip73Target::parse_podcast_episode_guid_i_value("abc-123"),
            Err(Nip73TargetError::MissingPodcastGuidPrefix)
        );
        // A prefix present but an empty suffix is still an empty GUID.
        assert_eq!(
            Nip73Target::parse_podcast_episode_guid_i_value("podcast:item:guid:"),
            Err(Nip73TargetError::EmptyValue)
        );
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
