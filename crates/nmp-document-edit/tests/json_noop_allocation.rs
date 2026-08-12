use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nmp_document_edit::{DocumentEditPlan, JsonFieldEdit, JsonMissing, Occurrences};

static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: allocation and deallocation are delegated unchanged to the system
// allocator. The atomics only observe requested allocation sizes.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: `layout` comes from the allocator caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` and `layout` came from the matching allocation.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn large_exact_json_set_does_not_allocate_a_parallel_document() {
    let mut source = String::from(r#"{"target":"exact""#);
    for index in 0..20_000 {
        source.push_str(&format!(r#", "key-{index}":"{}""#, "v".repeat(64)));
    }
    source.push('}');
    let plan = DocumentEditPlan::json_object(
        JsonFieldEdit::set(
            "target",
            r#""exact""#,
            Occurrences::All,
            JsonMissing::NoChange,
        )
        .unwrap(),
    );

    ALLOCATED.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    let outcome = plan.apply_json_object(&source).unwrap();
    TRACK.store(false, Ordering::Relaxed);
    let allocated = ALLOCATED.load(Ordering::Relaxed);

    assert_eq!(outcome.replacement, None);
    assert!(outcome.patches.is_empty());
    assert!(
        allocated < source.len() / 4,
        "a no-op allocated {allocated} bytes for a {}-byte document",
        source.len()
    );
}
