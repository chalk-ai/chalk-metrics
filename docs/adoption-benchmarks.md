# Adoption benchmarks

Baseline production revision: `96ae2f1dfe728a678556b9be71c80c54b17c2909`.
Measured on Linux x86_64, AMD EPYC 9B45 (16 logical CPUs), rustc 1.99.0
(`b940084d7`, 2026-09-28), release profile with the checked-in lockfile.
This is a shared virtual machine; contention and UDP timings are especially noisy.

The same benchmark harness is used on both implementations. Raw recording uses
fresh queues with 1,000 observations per batch (below the 16,384 limit), excluding
client startup and shutdown from timing. UDP receivers stay alive throughout
warmup and measurement. Raw UDP export measures actual observations, not sketch
means. Benchmarks run sequentially.

```sh
cargo bench --locked --bench metrics_throughput -- --warm-up-time 1 --measurement-time 2 --sample-size 20 --save-baseline upstream
cargo bench --locked --bench statsd_export -- --warm-up-time 1 --measurement-time 2 --sample-size 20 --save-baseline upstream
# After the relevant change, use --baseline upstream instead of --save-baseline upstream.
```

## Aggregation

Growth records 512 tag combinations into one stripe, then revisits each one:
1,024 recording calls per iteration. Startup, flush, and shutdown are excluded.
The baseline growth regressions produce 1,023–1,024 flushed entries instead of
512; corrected entries each contain counter total 100, gauge 99, or histogram
count 2 and sum 100. Gauges persist across flushes; counters and histograms reset.
The baseline performs incorrect duplicate allocations, so growth timing changes
must be read alongside correctness, not as equivalent-work speedups.

Existing-series benchmarks report time per 1,000 recording calls. Contention
benchmarks report 16,000 calls and include thread wakeup/join costs.

| Benchmark | Before (µs) | After (µs) | Change |
|---|---:|---:|---:|
| `growth/count` | 87.742 | 56.886 | -35.2% |
| `growth/gauge` | 86.292 | 56.230 | -34.8% |
| `growth/histogram` | 152.171 | 121.958 | -19.9% |
| `single_thread/histogram_static_tags` | 55.370 | 52.337 | -5.5% |
| `single_thread/histogram_mixed_tags` | 56.660 | 60.440 | +6.7% |
| `single_thread/count_mixed_tags` | 15.566 | 16.528 | +6.2% |
| `single_thread/count_static_tags` | 14.577 | 15.616 | +7.1% |
| `single_thread/gauge_mixed_tags` | 11.431 | 12.425 | +8.7% |
| `single_thread/histogram_raw_accepted` | 64.971 | 65.349 | +0.6% |
| `single_thread/gauge_static_tags` | 10.447 | 11.730 | +12.3% |
| `contention/histogram_mixed/8_threads` | 477.780 | 513.829 | +7.5% |
| `contention/histogram_mixed/2_threads` | 980.710 | 1069.449 | +9.0% |
| `contention/count_mixed/8_threads` | 265.033 | 210.782 | -20.5% |
| `contention/count_mixed/2_threads` | 263.426 | 268.304 | +1.9% |

The identity checks add roughly 6–12% to several counter/gauge update cases on
this host. Histogram and contention changes vary by workload. Growth avoids the
baseline's duplicate entries and allocations. These costs are reported rather
than hidden behind the incorrect baseline's throughput.

## Exporter-aware distribution routing

The behavioral baseline records `1` and `99` but sends one `latency:50|d`
observation. Correct routing sends `latency:1|d` and `latency:99|d`, once per
Distribution destination. Simultaneous sketches retain count 2 and sum 100.
Both legacy toggle settings are tested; neither can disable built-in requirements.
Directly exporting a sketch as Distribution now returns an error.

The table isolates routing changes against the aggregation PR (its parent), with
1,000 recording calls per batch. Raw observations all fit in fresh queues. The
cache now shares one tag allocation across the series rather than allocating
new tag data for every raw observation.

| Benchmark | Before (µs) | After (µs) | Change |
|---|---:|---:|---:|
| `single_thread/histogram_static_tags` | 52.337 | 53.362 | +2.0% |
| `single_thread/histogram_mixed_tags` | 60.440 | 61.297 | +1.4% |
| `single_thread/histogram_raw_accepted` | 65.349 | 46.057 | -29.5% |

## Transport hardening

These are the same formatter and UDP-send workloads on baseline and hardened
transport; each raw element is one original observation. Percentiles use batches
of count/gauge/histogram series. Timings measure formatting and send attempts,
not acknowledged receiver delivery. UDP is best effort.

| Benchmark | Before (µs) | After (µs) | Change |
|---|---:|---:|---:|
| `statsd_export/raw_distribution/1000` | 325.106 | 319.833 | -1.6% |
| `statsd_export/raw_distribution/10` | 4.255 | 4.331 | +1.8% |
| `statsd_export/raw_distribution/100` | 32.437 | 34.143 | +5.3% |
| `statsd_export/percentiles/1000` | 3783.912 | 3780.704 | -0.1% |
| `statsd_export/percentiles/10` | 36.857 | 35.724 | -3.1% |
| `statsd_export/percentiles/100` | 374.273 | 377.165 | +0.8% |

Before hardening, IPv6 construction fails, oversized individual lines exceed the
configured limit, zero-size buffers are accepted, and missing UDS destinations
silently succeed. Regression tests reproduce all four failures on the parent and
pass after hardening. Exact newline batching already worked and remains covered.
A two-sample queue accepts 1 and 99, rejects a third raw observation, reports one
drop, and leaves the mixed exporter's sketch at count 3/sum 105.

## Adoption checks

Tests exercise real IPv4/IPv6 datagrams, production formatting, byte boundaries
including UTF-8 tags, multiple destinations, periodic export on both runtime
configurations, exporter failure isolation, and UDS replacement. Explicit shutdown
before the first interval delivers all 400 accepted observations after four
producer threads join, exactly once. Repeated shutdown adds no observations.

Validation commands:

```sh
cargo fmt --all -- --check
cargo test --locked --workspace
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
cargo bench --locked --bench prometheus_export -- --warm-up-time 1 --measurement-time 2 --sample-size 20
```

[Machine-readable timing estimates](benchmark-results.json) preserve the upstream
baseline and each implementation stage. The verification PR changes tests,
documentation, and CI; it adds no production recording/export work.

Prometheus rendering is unchanged. Its final benchmark run completed for all
three batch sizes (10/100/1,000) and tag counts (0/2/5); final estimates are in
the machine-readable results. No before/after rendering claim is made.
