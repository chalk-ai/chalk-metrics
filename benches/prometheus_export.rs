use std::borrow::Cow;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::export::{FlushedMetric, FlushedValue, TagsData, UDDSketch};

fn make_count_metric(i: usize, num_tags: usize) -> FlushedMetric {
    let pairs: Vec<(&'static str, Cow<'static, str>)> = (0..num_tags)
        .map(|t| {
            let key: &'static str = match t {
                0 => "endpoint",
                1 => "status",
                2 => "region",
                3 => "service",
                _ => "extra",
            };
            (key, Cow::Owned(format!("value_{i}_{t}")))
        })
        .collect();

    FlushedMetric {
        namespace: &["http"],
        metric_name: "request_count",
        tags: Arc::new(TagsData { pairs }),
        value: FlushedValue::Count(i as i64 * 100),
    }
}

fn make_gauge_metric(i: usize, num_tags: usize) -> FlushedMetric {
    let pairs: Vec<(&'static str, Cow<'static, str>)> = (0..num_tags)
        .map(|t| {
            let key: &'static str = match t {
                0 => "endpoint",
                _ => "region",
            };
            (key, Cow::Owned(format!("value_{i}_{t}")))
        })
        .collect();

    FlushedMetric {
        namespace: &["http"],
        metric_name: "active_connections",
        tags: Arc::new(TagsData { pairs }),
        value: FlushedValue::Gauge(i as f64 * 1.5),
    }
}

fn make_histogram_metric(i: usize, num_tags: usize) -> FlushedMetric {
    let pairs: Vec<(&'static str, Cow<'static, str>)> = (0..num_tags)
        .map(|t| {
            let key: &'static str = match t {
                0 => "endpoint",
                1 => "status",
                _ => "region",
            };
            (key, Cow::Owned(format!("value_{i}_{t}")))
        })
        .collect();

    let mut sketch = UDDSketch::new(200, 0.001);
    for v in 0..100 {
        sketch.add_value(v as f64 * 0.01);
    }

    FlushedMetric {
        namespace: &["http"],
        metric_name: "request_latency",
        tags: Arc::new(TagsData { pairs }),
        value: FlushedValue::Histogram(sketch),
    }
}

fn make_mixed_batch(size: usize, num_tags: usize) -> Vec<FlushedMetric> {
    let mut metrics = Vec::with_capacity(size);
    for i in 0..size {
        match i % 3 {
            0 => metrics.push(make_count_metric(i, num_tags)),
            1 => metrics.push(make_gauge_metric(i, num_tags)),
            _ => metrics.push(make_histogram_metric(i, num_tags)),
        }
    }
    metrics
}

fn prometheus_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("prometheus_export");

    for batch_size in [10, 100, 1000] {
        group.throughput(Throughput::Elements(batch_size as u64));

        group.bench_with_input(
            BenchmarkId::new("mixed/2_tags", batch_size),
            &batch_size,
            |b, &size| {
                let exporter = PrometheusExporter::builder().namespace("myapp").build();
                let metrics = make_mixed_batch(size, 2);
                b.iter(|| {
                    let _ = exporter.render_metrics(&metrics);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mixed/5_tags", batch_size),
            &batch_size,
            |b, &size| {
                let exporter = PrometheusExporter::builder().namespace("myapp").build();
                let metrics = make_mixed_batch(size, 5);
                b.iter(|| {
                    let _ = exporter.render_metrics(&metrics);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mixed/0_tags", batch_size),
            &batch_size,
            |b, &size| {
                let exporter = PrometheusExporter::builder().build();
                let metrics = make_mixed_batch(size, 0);
                b.iter(|| {
                    let _ = exporter.render_metrics(&metrics);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, prometheus_benchmarks);
criterion_main!(benches);
