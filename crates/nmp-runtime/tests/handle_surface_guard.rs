//! `Handle`'s public surface, guarded by reading this crate's own source.
//!
//! Moved here from `nmp/tests/runtime_integration.rs` when the runtime became
//! its own crate (#1707). The guard scans the runtime's own sources, so it
//! belongs beside them: over there it resolved
//! `$CARGO_MANIFEST_DIR/src/runtime`, a directory that no longer exists, and
//! a path-based guard that cannot find its subject is worse than none.

/// Structural grep-guard (M3 plan §5 test 14, widened by M4/M5 and #3 U4):
/// `Handle`'s public surface is the original verbs plus diagnostics and the
/// stable-receipt operations (`publish`/bounded queue inspection/
/// `reattach_receipt` plus cursor-based `reattach_receipt_from` for finite
/// replay pages) and the
/// governed sign-only operation's blocking/completion doors -- no `relays:`
/// parameter, no open-REQ method anywhere on it
/// (ledger #2/#3 preserved at the top edge; `add_signer`/`remove_signer` are
/// M4's deliberate lifecycle widening, closing the multi-account and remote
/// signer detach gaps; `observe_diagnostics` is M5's --
/// read-only, off the data path, never influences routing/delivery). The two
/// `bench-instrumentation` methods expose only deterministic ownership and
/// deadline controls to the governed stress harness; they are reviewed here
/// explicitly rather than hidden from the source-level guard. Asserted
/// by reading this crate's own source rather than by reflection (Rust has
/// none) -- the same "grep-guard" idiom the plan itself names.
#[test]
fn handle_surface_is_closed_and_receipt_reattachment_is_explicit() {
    let runtime_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut pending_directories = vec![runtime_dir.clone()];
    let mut runtime_sources = Vec::new();
    while let Some(directory) = pending_directories.pop() {
        for entry in std::fs::read_dir(&directory).expect("read runtime source directory") {
            let path = entry.expect("read runtime source entry").path();
            if path.is_dir() {
                pending_directories.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path).expect("read runtime Rust source");
                runtime_sources.push((path, source));
            }
        }
    }
    runtime_sources.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

    let handle_impl_owners: Vec<_> = runtime_sources
        .iter()
        .filter_map(|(path, source)| {
            let count = source.match_indices("impl Handle {").count();
            (count != 0).then(|| {
                (
                    path.strip_prefix(&runtime_dir)
                        .expect("runtime source stays below runtime directory")
                        .to_string_lossy()
                        .into_owned(),
                    count,
                )
            })
        })
        .collect();
    // #1628: `sign_event.rs` is a reviewed owner, not an incidental third
    // file. It holds the sign-only lifecycle's whole state, so the two facade
    // verbs that hand back a registration derived from that state live with
    // it. A NEW name appearing in this list is the thing to object to.
    assert_eq!(
        handle_impl_owners,
        vec![
            ("lib.rs".to_owned(), 1),
            ("receipt_stream.rs".to_owned(), 1),
            ("sign_event.rs".to_owned(), 1)
        ],
        "Handle must have exactly one impl in each reviewed owner and none elsewhere"
    );

    let mut methods: Vec<&str> = Vec::new();
    for (_, src) in runtime_sources
        .iter()
        .filter(|(_, source)| source.contains("impl Handle {"))
    {
        let impl_block_start = src
            .find("impl Handle {")
            .expect("each Handle owner must have an impl block");
        let handle_impl = &src[impl_block_start..];
        let impl_block_end = handle_impl
            .find("\n}\n")
            .expect("Handle's impl block must close");
        let handle_impl = &handle_impl[..impl_block_end];
        for line in handle_impl.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("pub fn ") {
                let name = rest.split(['(', '<']).next().unwrap_or_default();
                methods.push(name);
            }
        }
    }
    methods.sort_unstable();
    // The eight session-lifecycle verbs below were `pub(crate)` while the
    // runtime and the facade shared one crate. They were always part of
    // `Handle`'s reviewed surface -- `nmp::Engine`'s session verbs are exactly
    // these, one call deep -- and the crate boundary is what forced them to
    // say so. Their capability did not widen; only their spelling did. A verb
    // appearing here that `Engine` does NOT drive is the thing to object to.
    let mut expected = vec![
        "add_auth_policy",
        "add_private_key_account",
        "add_public_key_account",
        "add_signer",
        "clear_session",
        "current_session_pubkey",
        "make_current_account",
        "remove_session_account",
        "session_export_sources",
        "session_snapshot",
        "bench_hold_due_deadline_command",
        "cancel_write",
        "observation_ownership_census",
        "observe_diagnostics",
        "publish",
        "publish_queue_entries",
        "publish_queue_entries_for_event",
        "reattach_receipt",
        "reattach_receipt_from",
        "receipt_result",
        "relay_information",
        "remove_auth_policy",
        "remove_publish_queue_entry",
        "remove_signer",
        "request_rows",
        "set_current_account",
        "shutdown",
        "sign_event",
        "sign_event_with_completion",
        "subscribe",
        "subscribe_history",
        "unsubscribe",
        "unsubscribe_history",
    ];
    expected.sort_unstable();
    assert_eq!(
        methods, expected,
        "Handle must expose only the reviewed verbs -- no relays:/open-REQ method"
    );

    // Scan CODE lines only (skip `///`/`//` doc/comment prose, which is
    // free to describe the absence of these things in words) for the actual
    // structural violations: a `relays:` parameter or an open-REQ method.
    let code_lines: Vec<&str> = runtime_sources
        .iter()
        .flat_map(|(_, source)| source.lines())
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .collect();
    assert!(
        !code_lines.iter().any(|line| {
            line.match_indices("relays:").any(|(index, _)| {
                index == 0 || {
                    let before = line.as_bytes()[index - 1];
                    !before.is_ascii_alphanumeric() && before != b'_'
                }
            })
        }),
        "no method signature on the runtime surface may take a bare `relays:` parameter"
    );
    assert!(
        !code_lines
            .iter()
            .any(|l| l.contains("fn open_req") || l.contains("fn open(")),
        "no open-REQ method may exist anywhere in the runtime module"
    );
}
