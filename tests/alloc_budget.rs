//! Heap allocations per call on the generated recording path.
//!
//! Allocation counts are exact and noise-free, so unlike timings they can gate
//! CI. The budgets below are what the library does today: a ratchet that makes
//! any change to the count visible in review, not a target. The one allocation
//! per `String` tag in the "built" cases is the caller constructing the tag
//! (`Endpoint::from("/api")`), which this crate's API requires.
//!
//! Allocations are counted per thread so the client's background worker thread
//! cannot pollute the count. Everything runs in one test because the global
//! client can be initialized only once per process and the uninitialized cases
//! must run first.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Duration;

use chalk_metrics::{client, define_metrics, define_namespaces, define_tags};

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// Counts `alloc` and `realloc` calls made by the current thread.
struct CountingAllocator;

impl CountingAllocator {
    fn count() {
        // `try_with` so an allocation during thread teardown cannot panic.
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

define_tags! {
    Status => "status" {
        Success => "success",
        Failure => "failure",
    }
    Method => "method" {
        Get => "get",
        Post => "post",
    }
    Endpoint => "endpoint";
    Region => "region";
    Tenant => "tenant";
}

define_namespaces! {
    Http => "http";
}

define_metrics! {
    group(tags = []) {
        count Count0Tags => "count_0_tags", "No tags";
        histogram Histogram0Tags => "histogram_0_tags", "No tags";
    }
    group(namespace = Http, tags = [Status]) {
        count Count1Enum => "count_1_enum", "One enum tag";
    }
    group(namespace = Http, tags = [Status, Method]) {
        count Count2Enum => "count_2_enum", "Two enum tags";
    }
    group(namespace = Http, tags = [Endpoint, Status]) {
        count CountStringEnum => "count_string_enum", "String and enum tags";
        gauge GaugeStringEnum => "gauge_string_enum", "String and enum tags";
        histogram HistogramStringEnum => "histogram_string_enum", "String and enum tags";
    }
    group(namespace = Http, tags = [Endpoint, Region, Tenant, Status, Method]) {
        count Count5Tags => "count_5_tags", "Three string and two enum tags";
    }
}

const WARM_UP_CALLS: u64 = 16;
const CALLS: u64 = 1000;

const ENDPOINT: &str = "/api/v1/users";
const REGION: &str = "us-east-1";
const TENANT: &str = "tenant-0042";
/// Constant, so sketch bucket growth cannot add allocations.
const LATENCY: f64 = 0.02;

fn count_5_tags() -> Count5Tags {
    Count5Tags {
        endpoint: Endpoint::from(black_box(ENDPOINT)),
        region: Region::from(black_box(REGION)),
        tenant: Tenant::from(black_box(TENANT)),
        status: Status::Success,
        method: Method::Post,
    }
}

/// Asserts that `call` allocates exactly `per_call` times per call, measured
/// over `CALLS` calls on this thread after a warm-up that creates the series.
#[track_caller]
fn assert_allocations(case: &str, per_call: u64, mut call: impl FnMut()) {
    for _ in 0..WARM_UP_CALLS {
        call();
    }
    let before = ALLOCATIONS.get();
    for _ in 0..CALLS {
        call();
    }
    let measured = ALLOCATIONS.get() - before;
    assert_eq!(
        measured,
        per_call * CALLS,
        "{case}: measured {measured} allocations over {CALLS} calls, but the budget is \
         {per_call} per call. If the change is intentional, update the budget in \
         tests/alloc_budget.rs.",
    );
}

#[test]
fn generated_recording_allocation_budget() {
    // Before `init()`: recording is a no-op, but the caller still builds the tags.
    assert_allocations("uninitialized count, 0 tags", 0, || Count0Tags {}.record());
    assert_allocations("uninitialized count, string + enum built", 1, || {
        CountStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record()
    });

    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .init();

    assert_allocations("count, 0 tags", 0, || Count0Tags {}.record());
    assert_allocations("count, 1 enum", 0, || {
        Count1Enum {
            status: black_box(Status::Success),
        }
        .record()
    });
    assert_allocations("count, 2 enums", 0, || {
        Count2Enum {
            status: black_box(Status::Failure),
            method: black_box(Method::Get),
        }
        .record()
    });
    let prebuilt = CountStringEnum {
        endpoint: Endpoint::from(ENDPOINT),
        status: Status::Success,
    };
    assert_allocations("count, string + enum prebuilt", 0, || {
        black_box(&prebuilt).record()
    });
    assert_allocations("count, string + enum built", 1, || {
        CountStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record()
    });
    let prebuilt_5_tags = count_5_tags();
    assert_allocations("count, 5 tags prebuilt", 0, || {
        black_box(&prebuilt_5_tags).record()
    });
    assert_allocations("count, 5 tags built", 3, || count_5_tags().record());
    assert_allocations("gauge, string + enum built", 1, || {
        GaugeStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record(black_box(42.0))
    });
    assert_allocations("histogram, 0 tags", 0, || {
        Histogram0Tags {}.record(black_box(LATENCY))
    });
    assert_allocations("histogram, string + enum built", 1, || {
        HistogramStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record(black_box(LATENCY))
    });

    client::shutdown();
}
