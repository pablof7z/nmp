use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nmp_store::{EventStore, MemoryStore, RedbStore, RefuseReason};
use nostr::{EventId, Keys, Timestamp};

struct MeasuringAllocator;

static MEASURING: AtomicBool = AtomicBool::new(false);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every allocation is delegated unchanged to the system allocator.
unsafe impl GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if MEASURING.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if MEASURING.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if MEASURING.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: MeasuringAllocator = MeasuringAllocator;

fn allocated_while(action: impl FnOnce()) -> u64 {
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    MEASURING.store(true, Ordering::Relaxed);
    action();
    MEASURING.store(false, Ordering::Relaxed);
    ALLOCATED_BYTES.load(Ordering::Relaxed)
}

#[test]
fn deadline_peek_and_prune_are_independent_of_unrelated_receipt_history() {
    let mut store = MemoryStore::new();
    let author = Keys::generate().public_key();
    for ordinal in 0..20_000u64 {
        store
            .accept_refused(
                EventId::from_byte_array(ordinal.to_be_bytes().repeat(4).try_into().unwrap()),
                author,
                RefuseReason::Tombstoned,
            )
            .unwrap();
    }

    let allocated = allocated_while(|| {
        assert_eq!(store.next_superseded_receipt_deadline().unwrap(), None);
        assert!(store
            .prune_superseded_receipts(Timestamp::from(10_000))
            .unwrap()
            .is_empty());
    });

    assert!(
        allocated < 4_096,
        "deadline work materialized unrelated retained receipts: {allocated} bytes allocated"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deadline-index.redb");
    let mut store = RedbStore::open(&path).unwrap();
    for ordinal in 0..2_000u64 {
        store
            .accept_refused(
                EventId::from_byte_array(ordinal.to_be_bytes().repeat(4).try_into().unwrap()),
                author,
                RefuseReason::Tombstoned,
            )
            .unwrap();
    }

    let allocated = allocated_while(|| {
        assert_eq!(store.next_superseded_receipt_deadline().unwrap(), None);
        assert!(store
            .prune_superseded_receipts(Timestamp::from(10_000))
            .unwrap()
            .is_empty());
    });

    assert!(
        allocated < 16_384,
        "Redb deadline work materialized unrelated retained receipts: {allocated} bytes allocated"
    );
}
