# Delivery-side app transforms

Once NMP has delivered a snapshot, its rows are ordinary app data. Use the
language and architecture you already have to rank, group, filter for
presentation, map into view models, or join with non-Nostr state.

NMP does not need a blessed `.filter`, `.sorted`, or `.map` wrapper for this.

```rust
while let Some(snapshot) = snapshots.recv() {
    let mut rows: Vec<_> = snapshot
        .rows
        .into_iter()
        .filter(|row| product_policy.admits(row))
        .collect();
    rows.sort_by(product_policy.order());
    app_state.apply(rows, snapshot.acquisition, snapshot.shortfall);
}
```

## The boundary

These transforms may decide presentation. They may not feed opaque code back
into:

- binding resolution;
- routing or authenticated identity;
- wire coalescing;
- persistence or replaceable winner selection;
- protocol validation;
- signer choice; or
- acquisition/pagination cursor correctness.

If a set is itself derivable from Nostr state and must change network demand,
express it with the closed binding grammar or an exact protocol-module query.
If it is product policy over rows NMP already delivered, keep it here.

---

<sub>[Index](README.md) · Related: [Snapshots and evidence](10-consuming-results.md) · [Binding grammar](09-binding-grammar.md) · [What NMP does not own](29-not-do.md)</sub>
