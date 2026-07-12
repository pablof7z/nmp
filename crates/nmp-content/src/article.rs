use nostr::Event;

/// Typed NIP-23 long-form content semantics. Reading time remains a native
/// presentation estimate derived from `content`, not a protocol field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Article {
    pub event_id: String,
    pub author: String,
    pub created_at: u64,
    pub identifier: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub image: Option<String>,
    pub published_at: Option<u64>,
    pub content: String,
}

pub fn decode_article(event: &Event) -> Article {
    decode_article_from_raw(
        &event.id.to_hex(),
        &event.pubkey.to_hex(),
        event.created_at.as_secs(),
        event.tags.iter().map(|tag| tag.as_slice()),
        &event.content,
    )
}

pub fn decode_article_from_raw<'a>(
    event_id: &str,
    author: &str,
    created_at: u64,
    tags: impl IntoIterator<Item = &'a [String]>,
    content: &str,
) -> Article {
    let mut article = Article {
        event_id: event_id.to_string(),
        author: author.to_string(),
        created_at,
        content: content.to_string(),
        ..Article::default()
    };
    for tag in tags {
        let Some(name) = tag.first().map(String::as_str) else {
            continue;
        };
        let value = tag.get(1).and_then(|value| nonempty(value));
        match name {
            "d" if article.identifier.is_empty() => {
                article.identifier = value.unwrap_or_default();
            }
            "title" if article.title.is_none() => article.title = value,
            "summary" if article.summary.is_none() => article.summary = value,
            "image" if article.image.is_none() => article.image = value,
            "published_at" if article.published_at.is_none() => {
                article.published_at = value.and_then(|value| value.parse().ok());
            }
            _ => {}
        }
    }
    article
}

fn nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_nip23_metadata_without_inventing_fallbacks() {
        let tags = [
            vec!["d".to_string(), "article-id".to_string()],
            vec!["title".to_string(), "A real article".to_string()],
            vec!["summary".to_string(), "Summary".to_string()],
            vec![
                "image".to_string(),
                "https://example.com/hero.jpg".to_string(),
            ],
            vec!["published_at".to_string(), "123".to_string()],
        ];
        let article = decode_article_from_raw(
            "event",
            "author",
            456,
            tags.iter().map(Vec::as_slice),
            "Body words",
        );
        assert_eq!(article.identifier, "article-id");
        assert_eq!(article.title.as_deref(), Some("A real article"));
        assert_eq!(article.summary.as_deref(), Some("Summary"));
        assert_eq!(article.published_at, Some(123));
        assert_eq!(article.created_at, 456);
    }

    #[test]
    fn invalid_published_time_stays_absent() {
        let tags = [vec!["published_at".to_string(), "tomorrow".to_string()]];
        let article =
            decode_article_from_raw("event", "author", 1, tags.iter().map(Vec::as_slice), "body");
        assert_eq!(article.published_at, None);
    }
}
