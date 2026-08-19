//! Interpolated content: one statement that writes what the reader sees AND
//! the rows that make it resolvable (#1243).
//!
//! ```ignore
//! EventBuilder::reply_to(&target)
//!     .content(text!("hey {}, check this out: {}", alice, event))
//! ```
//! ```text
//! content: "hey nostr:npub1…, check this out: nostr:nevent1qqs…"
//! ["p", "<alice hex>"]
//! ["q", "<event id>", "wss://…", "<event author>"]
//! ```
//!
//! **This is the structural fix for two defects at once.** Because the `q` row
//! and the inline `nostr:nevent1…` come out of one statement, they cannot
//! diverge — which is exactly what NIP-C7's old quote-shaped reply composer got
//! wrong (it emitted a `q` row with nothing in the content quoting it, so no
//! C7 client could render it as C7 intends), and the same for NIP-27 mentions
//! written into content with no matching `p` row.
//!
//! Bech32 appears in the rendered content and nowhere else — that is the user
//! boundary. Every macro argument stays a decoded [`nostr::PublicKey`] or an event; nothing here
//! takes an `npub` or an `nevent` as input.

use nostr::nips::nip19::{Nip19Event, Nip19Profile, ToBech32};
use nostr::{Event, RelayUrl, Tag};

/// A content value that knows which rows must accompany it.
///
/// [`crate::EventBuilder::content`] takes this rather than a bare string, so a
/// plain `&str`/`String` still works unchanged (`From` does nothing but wrap
/// it) while an interpolated one carries its rows along.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterpolatedContent {
    pub text: String,
    /// The rows the interpolations require. Appended by
    /// [`crate::EventBuilder::content`] after whatever the builder already
    /// carried, never reordered and never deduplicated against a row a
    /// composer stated for its own reasons.
    pub rows: Vec<Tag>,
}

impl From<String> for InterpolatedContent {
    fn from(text: String) -> Self {
        Self {
            text,
            rows: Vec::new(),
        }
    }
}

impl From<&str> for InterpolatedContent {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// Something that can be named inline. Implementations render the bech32 form
/// a human sees and the row that makes it resolvable, and they are written
/// once each so the two halves cannot disagree.
pub trait Mention {
    /// What the reader sees, including NIP-21's `nostr:` scheme.
    fn render(&self) -> String;
    /// The rows that make the rendered reference resolvable.
    fn rows(&self) -> Vec<Tag>;
}

impl Mention for nostr::PublicKey {
    fn render(&self) -> String {
        format!(
            "nostr:{}",
            self.to_bech32()
                .expect("a public key always renders as npub")
        )
    }

    fn rows(&self) -> Vec<Tag> {
        // No hint. A person's relay hint is an outbox fact (NIP-65), not
        // something an event carries, and this crate cannot reach NIP-65 --
        // so the slot is left honestly empty rather than filled with a guess.
        // Where person hints come from is deliberately still open.
        vec![Tag::parse(["p", &self.to_hex()]).expect("non-empty p row")]
    }
}

impl Mention for &nostr::PublicKey {
    fn render(&self) -> String {
        (*self).render()
    }
    fn rows(&self) -> Vec<Tag> {
        (*self).rows()
    }
}

/// An event named inline is a QUOTE, not a reply: NIP-18's `q` row exists
/// precisely so *"quote reposts are not pulled and included as replies in
/// threads"*. Naming one in content therefore never threads it.
impl Mention for Event {
    fn render(&self) -> String {
        let pointer = Nip19Event::new(self.id).author(self.pubkey).kind(self.kind);
        format!(
            "nostr:{}",
            pointer
                .to_bech32()
                .expect("an event pointer always renders as nevent")
        )
    }

    fn rows(&self) -> Vec<Tag> {
        vec![
            Tag::parse(["q", &self.id.to_hex(), "", &self.pubkey.to_hex()])
                .expect("non-empty q row"),
        ]
    }
}

impl Mention for &Event {
    fn render(&self) -> String {
        (*self).render()
    }
    fn rows(&self) -> Vec<Tag> {
        (*self).rows()
    }
}

/// A person plus the relay a reader should look at — what an app that already
/// resolved an outbox hint supplies, since [`Mention`] for a bare key
/// deliberately does not guess one.
pub struct At<T>(pub T, pub RelayUrl);

impl Mention for At<nostr::PublicKey> {
    fn render(&self) -> String {
        let profile = Nip19Profile::new(self.0, [self.1.clone()]);
        format!(
            "nostr:{}",
            profile
                .to_bech32()
                .expect("a profile pointer always renders as nprofile")
        )
    }

    fn rows(&self) -> Vec<Tag> {
        vec![Tag::parse(["p", &self.0.to_hex(), &self.1.to_string()]).expect("non-empty p row")]
    }
}

impl Mention for At<&Event> {
    fn render(&self) -> String {
        let pointer = Nip19Event::new(self.0.id)
            .author(self.0.pubkey)
            .kind(self.0.kind)
            .relays([self.1.clone()]);
        format!(
            "nostr:{}",
            pointer
                .to_bech32()
                .expect("an event pointer always renders as nevent")
        )
    }

    fn rows(&self) -> Vec<Tag> {
        vec![Tag::parse([
            "q",
            &self.0.id.to_hex(),
            &self.1.to_string(),
            &self.0.pubkey.to_hex(),
        ])
        .expect("non-empty q row")]
    }
}

/// Render one content string whose inline references and accompanying rows
/// come from the same statement and therefore cannot diverge.
///
/// A macro because Rust has no variadic `format!` as a function. It produces
/// the CONTENT VALUE rather than wrapping the whole call — `content!(builder,
/// …)` was considered and rejected, because a wrapping form cannot sit between
/// `reply_to` and a later combinator and so breaks the chain.
///
/// ```ignore
/// let content = text!("hey {}, look: {}", alice, &event);
/// ```
#[macro_export]
macro_rules! text {
    ($format:literal) => {
        $crate::InterpolatedContent::from(::std::format!($format))
    };
    ($format:literal, $($mention:expr),+ $(,)?) => {{
        let mut rows: ::std::vec::Vec<::nostr::Tag> = ::std::vec::Vec::new();
        let text = ::std::format!(
            $format,
            $({
                let mention = &$mention;
                rows.extend($crate::Mention::rows(mention));
                $crate::Mention::render(mention)
            }),+
        );
        $crate::InterpolatedContent { text, rows }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventBuilder;
    use nostr::{EventBuilder as NostrBuilder, Keys, Kind, Timestamp};

    fn note() -> Event {
        NostrBuilder::new(Kind::from(1u16), "quoted")
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&Keys::generate())
            .expect("test event signs")
    }

    fn rows(builder: &EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    /// The whole point: one statement, and the inline reference and its row
    /// are both produced by it, so they cannot disagree. This is the shape
    /// the retired C7 reply composer got wrong -- a `q` row with nothing in
    /// the content quoting it.
    #[test]
    fn an_inline_reference_and_its_row_come_from_one_statement() {
        let alice = Keys::generate().public_key();
        let quoted = note();
        let built = EventBuilder::new(Kind::from(1u16)).content(text!(
            "hey {}, check this out: {}",
            alice,
            &quoted
        ));

        assert!(
            built.content.contains("nostr:npub1"),
            "the person is rendered at the user boundary: {}",
            built.content
        );
        assert!(
            built.content.contains("nostr:nevent1"),
            "the event is rendered at the user boundary: {}",
            built.content
        );

        let emitted = rows(&built);
        assert_eq!(emitted[0], vec!["p".to_string(), alice.to_hex()]);
        assert_eq!(
            emitted[1],
            vec![
                "q".to_string(),
                quoted.id.to_hex(),
                String::new(),
                quoted.pubkey.to_hex()
            ],
            "an event named inline is a QUOTE, never a thread reply"
        );
    }

    /// A plain string is still a plain string: no rows, no ceremony.
    #[test]
    fn a_bare_string_content_carries_no_rows() {
        let built = EventBuilder::new(Kind::from(1u16)).content("just words");
        assert_eq!(built.content, "just words");
        assert!(built.tags.is_empty());
    }

    /// A hint, when the app has one, reaches BOTH halves -- the rendered
    /// pointer and the row -- because both come from the same value.
    #[test]
    fn a_stated_hint_reaches_the_rendered_pointer_and_the_row_together() {
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let quoted = note();
        let built = EventBuilder::new(Kind::from(1u16))
            .content(text!("look: {}", At(&quoted, relay.clone())));
        assert!(built.content.contains("nostr:nevent1"));
        assert_eq!(rows(&built)[0][2], relay.to_string());
    }

    /// Interpolation appends its rows after whatever the builder already
    /// carried, and never touches them.
    #[test]
    fn interpolated_rows_never_disturb_the_rows_a_composer_stated() {
        let quoted = note();
        let built = EventBuilder::new(Kind::from(1u16))
            .tag(Tag::parse(["h", "group-id"]).unwrap())
            .content(text!("look: {}", &quoted));
        let emitted = rows(&built);
        assert_eq!(emitted[0], vec!["h".to_string(), "group-id".to_string()]);
        assert_eq!(emitted[1][0], "q");
    }
}
