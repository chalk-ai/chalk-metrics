//! The same public-API benchmark source runs on upstream and both stack revisions.
use std::hint::black_box;
use std::time::Duration;

use chalk_metrics::{client, define_metrics};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

define_metrics! {
    group(tags = []) {
    count ShortCount => "c", "Short name";
    count LongCount => "service_request_processing_successfully_completed_total_with_a_long_metric_name", "Long name";
    gauge ShortGauge => "g", "Short name";
    gauge LongGauge => "service_request_processing_current_active_requests_with_a_long_metric_name", "Long name";
    }
}

fn name_recording(c: &mut Criterion) {
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .init();
    let mut group = c.benchmark_group("name_recording");
    group.throughput(Throughput::Elements(1000));
    let operations: [(&str, fn()); 8] = [
        ("generated/count/short", || ShortCount {}.record()),
        ("generated/count/long", || LongCount {}.record()),
        ("generated/gauge/short", || {
            ShortGauge {}.record(black_box(42.0))
        }),
        ("generated/gauge/long", || {
            LongGauge {}.record(black_box(42.0))
        }),
        ("runtime/count/short", || {
            client::record_count(black_box(ShortCount::NAME), &[], 42, Vec::new, 1)
        }),
        ("runtime/count/long", || {
            client::record_count(black_box(LongCount::NAME), &[], 42, Vec::new, 1)
        }),
        ("runtime/gauge/short", || {
            client::record_gauge(
                black_box(ShortGauge::NAME),
                &[],
                42,
                Vec::new,
                black_box(42.0),
            )
        }),
        ("runtime/gauge/long", || {
            client::record_gauge(
                black_box(LongGauge::NAME),
                &[],
                42,
                Vec::new,
                black_box(42.0),
            )
        }),
    ];
    for (name, operation) in operations {
        group.bench_function(name, |b| {
            b.iter(|| {
                for _ in 0..1000 {
                    operation();
                }
            })
        });
    }
    group.finish();
    client::shutdown();
}

criterion_group!(benches, name_recording);
criterion_main!(benches);
