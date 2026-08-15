//! `nmp-nip73` -- the NIP-73 external-content-id owner (#1258, extracted
//! from `nmp-nip22` where it started life as `target.rs` in #572).
//!
//! An external content id is a `(i, k)` pair naming something that is not a
//! Nostr event: a podcast episode, a web page, a book. Several NIPs consume
//! them and none of them owns them -- NIP-22 tags one as a comment root,
//! NIP-25's kind:17 external reaction *"MUST include NIP-73 external content
//! `k` + `i` tags"* -- so the ids live in their own crate rather than inside
//! whichever consumer happened to need them first.
//!
//! Deliberately small: variants exist when a proof case needs one, never
//! preemptively for each row of NIP-73's table. What earns a variant is
//! **canonicalisation this crate can actually perform**
//! ([`Nip73::PodcastEpisode`]'s required `podcast:item:guid:` prefix,
//! [`Nip73::Url`]'s normalisation); everything else is [`Nip73::General`],
//! which carries an already-canonical pair and validates only that both
//! cells are non-empty.

/// A validated NIP-73 external content id. `i_value`/`k_value` are the
/// canonical `I`/`i` and `K`/`k` tag payloads this id renders as -- private
/// on purpose (constructor-validated data only leaves through the accessors
/// [`Self::i_value`]/[`Self::k_value`], never a raw field a caller could
/// build with an unvalidated shortcut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip73 {
    /// NIP-73's podcast episode GUID id: the stored `String` is the BARE
    /// GUID (this variant's ergonomic constructor input); the wire `I`/`i`
    /// value [`Self::i_value`] renders is the full `podcast:item:guid:<guid>`
    /// string NIP-73's own table (and NIP-22's own podcast example) require
    /// -- `K` is always the fixed literal
    /// [`Nip73::PODCAST_EPISODE_GUID_KIND`].
    PodcastEpisode(String),
    /// NIP-73's `web` id: the stored `String` is the ALREADY-CANONICAL URL
    /// [`Nip73::url`] produced, and it is also the wire `I`/`i` value
    /// verbatim. NIP-73's table specifies *"URL, normalized, no fragment"*,
    /// so canonicalisation is this variant's whole job -- two people naming
    /// the same page with `HTTPS://Example.COM:443/post#intro` and
    /// `https://example.com/post` must land on one thread, and a
    /// non-empty check would silently give them two.
    Url(String),
    /// Any other NIP-73 external id: a caller-supplied, ALREADY
    /// canonicalized `(value, kind)` pair. This crate does not know how to
    /// canonicalize namespaces it doesn't own -- validation here is
    /// exactly "both cells are non-empty", never a namespace-specific
    /// format check.
    ///
    /// This is also what a decoder produces when it reads an `I`/`K` pair
    /// naming a namespace with no variant here, which is why it is not an
    /// escape hatch that could be deleted once the table is enumerated:
    /// without it NMP could not decode valid events other clients publish.
    General { value: String, kind: String },
}

/// [`Nip73`] construction's typed refusal. Exhaustive; every variant is
/// constructed by a test, so none is dead surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip73Error {
    /// The `I` value was empty.
    EmptyValue,
    /// The `K` value was empty (general ids only -- the podcast and web
    /// variants' `K` is a fixed non-empty literal and can never trigger
    /// this).
    EmptyKind,
    /// A `K`/`k` cell of [`Nip73::PODCAST_EPISODE_GUID_KIND`] declared an
    /// `I`/`i` value that did NOT carry the required `podcast:item:guid:`
    /// prefix -- a decode-time-only refusal (never reachable through the
    /// ergonomic constructors, which always render the prefix themselves).
    MissingPodcastGuidPrefix,
    /// [`Nip73::url`] was handed something that is not an absolute URL, so
    /// there was nothing to normalise. A typed refusal rather than a
    /// best-effort passthrough: an un-normalised `web` value splits the
    /// thread for the page it names, which is precisely the failure this
    /// variant exists to prevent.
    MalformedUrl,
}

impl std::fmt::Display for Nip73Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyValue => f.write_str("NIP-73 external content id value must not be empty"),
            Self::EmptyKind => f.write_str("NIP-73 external content id kind must not be empty"),
            Self::MissingPodcastGuidPrefix => f.write_str(
                "podcast-episode-guid I/i value is missing its podcast:item:guid: prefix",
            ),
            Self::MalformedUrl => {
                f.write_str("NIP-73 web id is not an absolute URL and cannot be normalised")
            }
        }
    }
}

impl std::error::Error for Nip73Error {}

impl Nip73 {
    /// NIP-73's canonical `K` value for a podcast episode GUID.
    pub const PODCAST_EPISODE_GUID_KIND: &'static str = "podcast:item:guid";

    /// NIP-73's canonical `K` value for a web page.
    pub const WEB_KIND: &'static str = "web";

    /// NIP-73's canonical `I`/`i` value PREFIX for a podcast episode GUID --
    /// the wire value is `podcast:item:guid:<guid>`, NEVER the bare GUID
    /// (NIP-73's own table, and NIP-22's own podcast example, both use the
    /// prefixed form; a bare GUID is non-conformant and silently splits an
    /// episode's thread from conformant clients that only ever look for the
    /// prefixed value).
    const PODCAST_EPISODE_GUID_I_PREFIX: &'static str = "podcast:item:guid:";

    /// Construct a podcast-episode id from its bare GUID (never the
    /// prefixed wire value -- see [`Self::i_value`]). Refuses an empty GUID.
    pub fn podcast_episode(guid: &str) -> Result<Self, Nip73Error> {
        if guid.is_empty() {
            return Err(Nip73Error::EmptyValue);
        }
        Ok(Self::PodcastEpisode(guid.to_string()))
    }

    /// Construct a `web` id, normalising as NIP-73's table requires
    /// (*"URL, normalized, no fragment"*).
    ///
    /// Normalisation is real, not cosmetic: the input is parsed as an
    /// absolute URL, which lowercases the scheme and host, applies IDNA to
    /// a non-ASCII host, drops a port that is the scheme's default, and
    /// supplies the empty path -- and then the fragment is dropped
    /// outright, because a fragment names a position inside the page rather
    /// than a different page. What is deliberately NOT touched is the query
    /// string and the path case: both are server-significant, and dropping
    /// or folding either would merge two genuinely different pages into one
    /// thread, which is worse than the split it would fix.
    ///
    /// A relative or otherwise unparseable input is [`Nip73Error::MalformedUrl`]
    /// rather than a passthrough, since the whole value of this variant over
    /// [`Self::general`] is that its output IS canonical.
    pub fn url(url: &str) -> Result<Self, Nip73Error> {
        let mut parsed = url::Url::parse(url).map_err(|_| Nip73Error::MalformedUrl)?;
        parsed.set_fragment(None);
        Ok(Self::Url(parsed.to_string()))
    }

    /// Parse an already-decoded `I`/`i` value that a `K`/`k` cell of
    /// [`Self::PODCAST_EPISODE_GUID_KIND`] declares -- the wire value MUST
    /// carry the [`Self::PODCAST_EPISODE_GUID_I_PREFIX`] prefix; a value
    /// that doesn't (e.g. a bare GUID some other composer wrote) is a typed
    /// refusal, never silently reinterpreted.
    ///
    /// `pub` rather than `pub(crate)` since #1258 moved the decoder that
    /// calls it (`nmp-nip22`) across a crate boundary. There is
    /// deliberately no sibling `parse_web_i_value`: see [`Self::url`]'s
    /// note on why a read never re-canonicalises.
    pub fn parse_podcast_episode_guid_i_value(i_value: &str) -> Result<Self, Nip73Error> {
        let guid = i_value
            .strip_prefix(Self::PODCAST_EPISODE_GUID_I_PREFIX)
            .ok_or(Nip73Error::MissingPodcastGuidPrefix)?;
        Self::podcast_episode(guid)
    }

    /// Construct a general external id from an ALREADY-canonicalized
    /// `(value, kind)` pair. This crate does not own or validate any
    /// namespace's canonicalization rules beyond "non-empty" -- a caller
    /// composing e.g. an ISBN id owns getting that value/kind pair canonical
    /// before calling this.
    pub fn general(value: &str, kind: &str) -> Result<Self, Nip73Error> {
        if value.is_empty() {
            return Err(Nip73Error::EmptyValue);
        }
        if kind.is_empty() {
            return Err(Nip73Error::EmptyKind);
        }
        Ok(Self::General {
            value: value.to_string(),
            kind: kind.to_string(),
        })
    }

    /// The canonical `I`/`i` tag payload. For [`Self::PodcastEpisode`] this
    /// is NIP-73's full `podcast:item:guid:<guid>` wire value -- NOT the
    /// bare GUID [`Self::podcast_episode`] takes as input.
    pub fn i_value(&self) -> String {
        match self {
            Self::PodcastEpisode(guid) => {
                format!("{}{guid}", Self::PODCAST_EPISODE_GUID_I_PREFIX)
            }
            Self::Url(url) => url.clone(),
            Self::General { value, .. } => value.clone(),
        }
    }

    /// The canonical `K`/`k` tag payload.
    pub fn k_value(&self) -> &str {
        match self {
            Self::PodcastEpisode(_) => Self::PODCAST_EPISODE_GUID_KIND,
            Self::Url(_) => Self::WEB_KIND,
            Self::General { kind, .. } => kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `Nip73Error` variant is constructed across this module's
    /// tests, so none is dead surface; this one covers `EmptyValue` and the
    /// podcast round trip.
    #[test]
    fn podcast_episode_refuses_empty_and_round_trips_i_and_k() {
        assert_eq!(Nip73::podcast_episode(""), Err(Nip73Error::EmptyValue));
        let id = Nip73::podcast_episode("abc-123").unwrap();
        // NIP-73's wire `I`/`i` value is the FULL prefixed string, never
        // the bare GUID the ergonomic constructor takes.
        assert_eq!(id.i_value(), "podcast:item:guid:abc-123");
        assert_eq!(id.k_value(), "podcast:item:guid");
    }

    /// `parse_podcast_episode_guid_i_value` round-trips a conformant wire
    /// value and refuses one missing the required prefix (the decode-time
    /// door `Nip73Error::MissingPodcastGuidPrefix` exists for).
    #[test]
    fn parse_podcast_episode_guid_i_value_requires_the_prefix() {
        let id = Nip73::parse_podcast_episode_guid_i_value("podcast:item:guid:abc-123").unwrap();
        assert_eq!(id, Nip73::podcast_episode("abc-123").unwrap());
        assert_eq!(
            Nip73::parse_podcast_episode_guid_i_value("abc-123"),
            Err(Nip73Error::MissingPodcastGuidPrefix)
        );
        // A prefix present but an empty suffix is still an empty GUID.
        assert_eq!(
            Nip73::parse_podcast_episode_guid_i_value("podcast:item:guid:"),
            Err(Nip73Error::EmptyValue)
        );
    }

    #[test]
    fn general_id_refuses_either_empty_cell_and_round_trips() {
        assert_eq!(Nip73::general("", "isbn"), Err(Nip73Error::EmptyValue));
        assert_eq!(Nip73::general("978-0", ""), Err(Nip73Error::EmptyKind));
        let id = Nip73::general("978-0-13-468599-1", "isbn").unwrap();
        assert_eq!(id.i_value(), "978-0-13-468599-1");
        assert_eq!(id.k_value(), "isbn");
    }

    /// The `Url` variant's whole justification: its constructor
    /// CANONICALISES, so several spellings of one page produce one `i`
    /// value and therefore one thread. A non-empty check would produce
    /// four different threads for the four inputs below.
    #[test]
    fn url_normalises_and_drops_the_fragment() {
        let canonical = Nip73::url("https://example.com/post/2").unwrap();
        assert_eq!(canonical.i_value(), "https://example.com/post/2");
        assert_eq!(canonical.k_value(), "web");

        for spelling in [
            "https://example.com/post/2#intro",
            "HTTPS://Example.COM/post/2",
            "https://example.com:443/post/2",
            "https://example.com/post/2#",
        ] {
            assert_eq!(
                Nip73::url(spelling).unwrap(),
                canonical,
                "{spelling} names the same page and must canonicalise to it"
            );
        }
    }

    /// What normalisation deliberately does NOT do. A query string and the
    /// path's case are server-significant: folding either would merge two
    /// genuinely different pages into one thread, which is a worse error
    /// than the split it would fix.
    #[test]
    fn url_keeps_server_significant_parts_distinct() {
        let bare = Nip73::url("https://example.com/Post").unwrap();
        assert_ne!(bare, Nip73::url("https://example.com/post").unwrap());
        assert_ne!(bare, Nip73::url("https://example.com/Post?page=2").unwrap());
        assert_eq!(
            Nip73::url("https://example.com/Post?page=2")
                .unwrap()
                .i_value(),
            "https://example.com/Post?page=2"
        );
    }

    /// Proves `MalformedUrl` is constructed, not dead surface. A relative
    /// reference has no scheme and no host, so there is nothing to
    /// normalise and nothing that could identify a page globally.
    #[test]
    fn url_refuses_anything_it_cannot_normalise() {
        for not_a_url in ["", "/post/2", "example.com/post", "not a url at all"] {
            assert_eq!(
                Nip73::url(not_a_url),
                Err(Nip73Error::MalformedUrl),
                "{not_a_url} is not an absolute URL"
            );
        }
    }
}

/// An external content id is not a Nostr event, so it has no tags, no author
/// and no thread of its own: **it is always the root scope**. The four-case
/// thread-position reading applies to the event case and is untouched here.
///
/// This impl is the whole reason the ids left `nmp-nip22`: `nmp-grammar` is
/// `generic-value` and may not reach a protocol crate, so grammar defines
/// [`nmp_grammar::RootScope`] naming no NIP and the protocol crate implements
/// it downward. `EventBuilder::reply_to(&Nip73::url("https://google.com")?)`
/// is what that buys.
impl nmp_grammar::RootScope for Nip73 {
    fn root_rows(&self, _options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        vec![
            nostr::Tag::parse(["I", &self.i_value()]).expect("non-empty I row"),
            nostr::Tag::parse(["K", self.k_value()]).expect("non-empty K row"),
        ]
    }

    fn parent_rows(&self, _options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        vec![
            nostr::Tag::parse(["i", &self.i_value()]).expect("non-empty i row"),
            nostr::Tag::parse(["k", self.k_value()]).expect("non-empty k row"),
        ]
    }

    /// `None`: this is not a Nostr event and has no kind. A reply to it is a
    /// NIP-22 comment, which is what [`nmp_grammar::RootScope::reply_kind`]'s
    /// default already says.
    fn entity_kind(&self) -> Option<nostr::Kind> {
        None
    }
}

#[cfg(test)]
mod root_scope_tests {
    use super::*;
    use nmp_grammar::{reply_to, RootScope, TagOptions};

    fn rows(builder: &nmp_grammar::EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    /// The maintainer's own example. An external content id is a legal reply
    /// target, it produces a NIP-22 comment (it has no kind, so it is never a
    /// text note), and the root scope is uppercase with no marker anywhere.
    #[test]
    fn an_external_content_id_is_a_reply_target_and_is_always_the_root() {
        let page = Nip73::url("https://google.com").unwrap();
        let comment = reply_to(&page);
        assert_eq!(comment.kind, nostr::Kind::from(1111u16));
        assert_eq!(
            rows(&comment),
            vec![
                vec!["I".to_string(), "https://google.com/".to_string()],
                vec!["K".to_string(), "web".to_string()],
                vec!["i".to_string(), "https://google.com/".to_string()],
                vec!["k".to_string(), "web".to_string()],
            ]
        );
    }

    /// It has no author and no thread, so no modifier can change what it
    /// emits -- there is nothing for `without_author` or a relay hint to act
    /// on.
    #[test]
    fn an_external_content_id_has_nothing_for_a_modifier_to_change() {
        let episode = Nip73::podcast_episode("guid-1").unwrap();
        let plain = episode.parent_rows(&TagOptions::default());
        let modified = episode.parent_rows(
            &TagOptions::default()
                .without_author()
                .without_carried_mentions(),
        );
        assert_eq!(plain, modified);
        assert_eq!(episode.entity_kind(), None);
    }
}
