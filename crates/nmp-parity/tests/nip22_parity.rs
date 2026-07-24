//! #609: execute one NIP-22 reply compose/decode scenario through the direct
//! Rust protocol API and the opaque FFI engine door. The FFI composed intent
//! stays take-once and unreadable: its canonical pending row is the observable
//! result of the real acceptance path, not a test-only inspection seam.

use std::collections::BTreeSet;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use nmp::{
    Binding, Durability, Engine, EngineConfig, Filter, LiveQuery, Row, RowDelta, Timestamp,
    WritePayload, WriteRouting,
};
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::nip22::{FfiCommentParent, FfiCommentRoot, FfiDecodedComment, FfiNip73Target};
use nmp_ffi::types::{FfiBinding, FfiFilter, FfiRow, FfiRowDelta};
use nmp_grammar::CorrelationToken;
use nmp_nip22::{CommentParent, CommentRoot, DecodedComment, Nip73Target};
use nostr::{EventId, Keys};

const WAIT: Duration = Duration::from_secs(10);
const AUTHOR_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const PARENT_AUTHOR_SECRET: &str =
    "0000000000000000000000000000000000000000000000000000000000000002";
const CREATED_AT: u64 = 1_700_000_609;
const CONTENT: &str = "direct and FFI NIP-22 parity";
const GUID: &str = "episode-guid-609";
const CORRELATION: &str = "nip22-parity-609";

#[derive(Debug, PartialEq, Eq)]
struct NormalizedCommentEvent {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedDecodedComment {
    event_id: String,
    author_pubkey: String,
    created_at: u64,
    content: String,
    root: NormalizedRoot,
    parent: NormalizedParent,
}

#[derive(Debug, PartialEq, Eq)]
enum NormalizedRoot {
    Event {
        event_id: String,
        kind: u16,
        author_pubkey: Option<String>,
    },
    Address {
        author_pubkey: String,
        kind: u16,
        identifier: String,
        event_id: Option<String>,
    },
    External {
        value: String,
        kind: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum NormalizedParent {
    Root,
    Comment {
        event_id: String,
        author_pubkey: Option<String>,
    },
}

#[tokio::test(flavor = "multi_thread")]
async fn nip22_reply_compose_and_decode_match_direct_rust_and_ffi() {
    let author = Keys::parse(AUTHOR_SECRET)
        .expect("fixed author key")
        .public_key();
    let parent_author = Keys::parse(PARENT_AUTHOR_SECRET)
        .expect("fixed parent-author key")
        .public_key();
    let parent_event_id = EventId::from_slice(&[2; 32]).expect("fixed parent event id");

    let direct_root =
        CommentRoot::External(Nip73Target::podcast_episode_guid(GUID).expect("valid GUID"));
    let direct_parent = CommentParent::Comment {
        event_id: parent_event_id,
        author: Some(parent_author),
    };
    let direct_intent = nmp_nip22::comment_intent(
        &direct_root,
        direct_parent,
        author,
        Timestamp::from(CREATED_AT),
        CONTENT.to_string(),
        Some(CorrelationToken::try_from(CORRELATION).expect("bounded correlation")),
    );
    assert_eq!(direct_intent.durability, Durability::Durable);
    assert!(matches!(direct_intent.routing, WriteRouting::AuthorOutbox));
    assert!(direct_intent.identity_override.is_none());
    assert_eq!(
        direct_intent.correlation.as_ref().map(ToString::to_string),
        Some(CORRELATION.to_string())
    );
    let expected_event = match &direct_intent.payload {
        WritePayload::Unsigned(unsigned) => normalize_unsigned(unsigned),
        WritePayload::Signed(_) => {
            panic!("NIP-22 direct composition must remain unsigned")
        }
        WritePayload::UnsignedReplaceableEdit { .. } => {
            panic!("NIP-22 direct composition must not be a replaceable edit")
        }
    };

    let direct_engine = Engine::new(EngineConfig::default()).expect("direct engine");
    direct_engine
        .set_active_account(Some(author))
        .expect("direct active account");
    direct_engine
        .publish_tracked(direct_intent)
        .expect("direct composed intent must be accepted");
    let direct_row = wait_for_direct_pending_row(&direct_engine, &author.to_hex());
    let direct_event = normalize_direct_row(&direct_row);
    assert_eq!(direct_event, expected_event);

    let ffi_engine = NmpEngine::new(NmpEngineConfig::default()).expect("FFI engine");
    ffi_engine
        .set_active_account(Some(author.to_hex()))
        .expect("FFI active account");
    let ffi_intent = ffi_engine
        .comment_intent(
            FfiCommentRoot::External {
                target: FfiNip73Target::PodcastEpisodeGuid {
                    guid: GUID.to_string(),
                },
            },
            FfiCommentParent::Comment {
                event_id: parent_event_id.to_hex(),
                author_pubkey: Some(parent_author.to_hex()),
            },
            author.to_hex(),
            CREATED_AT,
            CONTENT.to_string(),
            Some(CORRELATION.to_string()),
        )
        .expect("FFI semantic inputs must compose");
    ffi_engine
        .publish_composed(ffi_intent)
        .expect("FFI composed intent must be accepted");
    let ffi_row = wait_for_ffi_pending_row(&ffi_engine, &author.to_hex());
    let ffi_event = normalize_ffi_row(&ffi_row);

    assert_eq!(ffi_event, expected_event);
    assert_eq!(ffi_event, direct_event);

    let direct_decoded = nmp_nip22::decode_comment(
        direct_row.event.id,
        direct_row.event.pubkey,
        direct_row.event.created_at.as_secs(),
        direct_row.event.kind.as_u16(),
        &direct_row
            .event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>(),
        &direct_row.event.content,
    )
    .expect("direct pending row must decode");
    let ffi_decoded = nmp_ffi::nip22::decode_comment(ffi_row).expect("FFI pending row must decode");

    assert_eq!(
        normalize_ffi_decoded(ffi_decoded),
        normalize_direct_decoded(direct_decoded)
    );

    direct_engine.shutdown();
    ffi_engine.shutdown();
}

fn wait_for_direct_pending_row(engine: &Engine, author: &str) -> Row {
    let subscription = engine
        .observe(
            LiveQuery::from_filter(Filter {
                kinds: Some(BTreeSet::from([1111])),
                authors: Some(Binding::Literal(BTreeSet::from([author.to_string()]))),
                ..Filter::default()
            }),
            None,
        )
        .expect("direct NIP-22 observation");
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "direct pending row timed out");
        let frame = subscription
            .recv_timeout(remaining)
            .expect("direct pending-row frame");
        for delta in frame.deltas {
            if let RowDelta::Added(row) = delta {
                if row.event.kind.as_u16() == 1111 && row.event.content == CONTENT {
                    return row;
                }
            }
        }
    }
}

fn wait_for_ffi_pending_row(engine: &NmpEngine, author: &str) -> FfiRow {
    let stream = engine
        .observe(
            FfiFilter {
                kinds: Some(vec![1111]),
                authors: Some(FfiBinding::Literal {
                    values: vec![author.to_string()],
                }),
                ..FfiFilter::default()
            },
            None,
        )
        .expect("FFI NIP-22 observation");
    let (tx, rx) = mpsc::channel();
    let drain = Arc::clone(&stream);
    tokio::spawn(async move {
        while let Ok(Some(frame)) = drain.next().await {
            if tx.send(frame.deltas).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "FFI pending row timed out");
        let deltas = rx.recv_timeout(remaining).expect("FFI pending-row frame");
        for delta in deltas {
            if let FfiRowDelta::Added { row } = delta {
                if row.kind == 1111 && row.content == CONTENT {
                    return row;
                }
            }
        }
    }
}

fn normalize_unsigned(unsigned: &nostr::UnsignedEvent) -> NormalizedCommentEvent {
    NormalizedCommentEvent {
        id: unsigned.id.expect("composed event id").to_hex(),
        pubkey: unsigned.pubkey.to_hex(),
        created_at: unsigned.created_at.as_secs(),
        kind: unsigned.kind.as_u16(),
        tags: unsigned
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: unsigned.content.clone(),
    }
}

fn normalize_direct_row(row: &Row) -> NormalizedCommentEvent {
    NormalizedCommentEvent {
        id: row.event.id.to_hex(),
        pubkey: row.event.pubkey.to_hex(),
        created_at: row.event.created_at.as_secs(),
        kind: row.event.kind.as_u16(),
        tags: row
            .event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: row.event.content.clone(),
    }
}

fn normalize_ffi_row(row: &FfiRow) -> NormalizedCommentEvent {
    NormalizedCommentEvent {
        id: row.id.clone(),
        pubkey: row.pubkey.clone(),
        created_at: row.created_at,
        kind: row.kind,
        tags: row.tags.clone(),
        content: row.content.clone(),
    }
}

fn normalize_direct_decoded(decoded: DecodedComment) -> NormalizedDecodedComment {
    NormalizedDecodedComment {
        event_id: decoded.event_id.to_hex(),
        author_pubkey: decoded.author.to_hex(),
        created_at: decoded.created_at,
        content: decoded.content,
        root: match decoded.root {
            CommentRoot::Event {
                event_id,
                kind,
                author,
            } => NormalizedRoot::Event {
                event_id: event_id.to_hex(),
                kind,
                author_pubkey: author.map(|key| key.to_hex()),
            },
            CommentRoot::Address {
                author,
                kind,
                identifier,
                event_id,
            } => NormalizedRoot::Address {
                author_pubkey: author.to_hex(),
                kind,
                identifier,
                event_id: event_id.map(|id| id.to_hex()),
            },
            CommentRoot::External(target) => NormalizedRoot::External {
                value: target.i_value(),
                kind: target.k_value().to_string(),
            },
        },
        parent: match decoded.parent {
            CommentParent::Root => NormalizedParent::Root,
            CommentParent::Comment { event_id, author } => NormalizedParent::Comment {
                event_id: event_id.to_hex(),
                author_pubkey: author.map(|key| key.to_hex()),
            },
        },
    }
}

fn normalize_ffi_decoded(decoded: FfiDecodedComment) -> NormalizedDecodedComment {
    NormalizedDecodedComment {
        event_id: decoded.event_id,
        author_pubkey: decoded.author_pubkey,
        created_at: decoded.created_at,
        content: decoded.content,
        root: match decoded.root {
            FfiCommentRoot::Event {
                event_id,
                kind,
                author_pubkey,
            } => NormalizedRoot::Event {
                event_id,
                kind,
                author_pubkey,
            },
            FfiCommentRoot::Address {
                author_pubkey,
                kind,
                identifier,
                event_id,
            } => NormalizedRoot::Address {
                author_pubkey,
                kind,
                identifier,
                event_id,
            },
            FfiCommentRoot::External { target } => match target {
                FfiNip73Target::PodcastEpisodeGuid { guid } => NormalizedRoot::External {
                    value: format!("podcast:item:guid:{guid}"),
                    kind: Nip73Target::PODCAST_EPISODE_GUID_KIND.to_string(),
                },
                FfiNip73Target::General { value, kind } => NormalizedRoot::External { value, kind },
            },
        },
        parent: match decoded.parent {
            FfiCommentParent::Root => NormalizedParent::Root,
            FfiCommentParent::Comment {
                event_id,
                author_pubkey,
            } => NormalizedParent::Comment {
                event_id,
                author_pubkey,
            },
        },
    }
}
