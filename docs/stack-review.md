# Stack review revision

This revision compares upstream `96ae2f1`, the previously published stack
`7618737`, and revised production head `cb83017` (benchmark harness `353d971`).
The earlier [distribution volume report](distribution-volume.md) compares a
different, pre-packing published baseline (`22ae1f9`); keep those comparisons separate.

## Name hashing and aggregation

Generated metrics now contain a private, compile-time content-based name
descriptor. Its fields cannot be forged independently. Existing string recording
APIs compute the same fingerprint at runtime; both paths share series identity,
including equal strings at different addresses. HashMap owns bucket hashing on
lookup, insertion, and growth. Namespace, name, kind, and tag fingerprint still
participate in equality. New tests combine both paths across forced growth and
exercise actual generated count, gauge, and histogram recording.

`benches/name_recording.rs` is identical on all three versions and exercises the
generated global API plus runtime string calls with `black_box` names. Each sample
contains 1,000 calls, including the same function-pointer dispatch overhead.
Names are 1 byte for short names and 79/74 bytes for long count/gauge names.
Existing recording, growth, and contention harnesses are also rerun unchanged;
upstream omits only the unavailable post-timing queue-drop assertion.

Linux x86_64, AMD EPYC 9B45, 16 logical CPUs, rustc 1.99.0, locked release build.
Criterion: 1 s warmup, 2 s measurement, 20 samples. Runs are sequential on a shared
VM. Values are mean microseconds per batch; intervals are in the JSON artifact.

| Workload | Upstream (µs) | Published (µs) | Revised (µs) | Revised vs published |
|---|---:|---:|---:|---:|
| `growth/count` | 88.250 | 57.901 | 58.622 | +1.2% |
| `growth/gauge` | 87.017 | 56.439 | 57.308 | +1.5% |
| `growth/histogram` | 151.485 | 129.098 | 128.163 | -0.7% |
| `single_thread/count_mixed_tags` | 15.575 | 16.543 | 16.694 | +0.9% |
| `single_thread/count_static_tags` | 14.582 | 15.561 | 15.557 | -0.0% |
| `single_thread/gauge_mixed_tags` | 11.433 | 12.407 | 12.396 | -0.1% |
| `single_thread/histogram_raw_accepted` | 65.336 | 45.821 | 46.446 | +1.4% |
| `single_thread/gauge_static_tags` | 10.450 | 11.633 | 11.641 | +0.1% |
| `name_recording/generated_gauge_long` | 11.666 | 15.562 | 15.568 | +0.0% |
| `name_recording/generated_gauge_short` | 11.667 | 15.586 | 15.571 | -0.1% |
| `name_recording/runtime_count_short` | 16.055 | 19.938 | 19.976 | +0.2% |
| `name_recording/generated_count_long` | 15.549 | 19.268 | 19.273 | +0.0% |
| `name_recording/runtime_gauge_short` | 12.399 | 16.040 | 16.031 | -0.1% |
| `name_recording/runtime_gauge_long` | 12.394 | 56.130 | 56.406 | +0.5% |
| `name_recording/generated_count_short` | 15.554 | 19.443 | 19.441 | -0.0% |
| `name_recording/runtime_count_long` | 16.036 | 60.598 | 60.112 | -0.8% |

The generated short/long paths show no material improvement from precomputation
on this compiler. The revised generated counter is approximately 19.3–19.4 µs
and gauge 15.6 µs per 1,000 calls, essentially unchanged from published.
Long names were already constant-folded in the generated path; optimized assembly
from both published and revised versions uses a constant placement fingerprint. Runtime string calls still hash content
and cost approximately 60 µs/1,000 long-name counter calls. The name-hashing
hypothesis does not explain away the original counter/gauge regression. Existing
local counter/gauge cases remain approximately 7–11% slower than upstream.
The generated counter/gauge cases in this harness are approximately 25%/33% slower
than upstream; these costs are still present and are not attributed to name hashing.
The descriptor guarantees precomputation rather than relying on optimizer
propagation; it is not claimed as an observed throughput improvement.

Growth timings are not equivalent-work speedups over upstream: upstream emits
1,023–1,024 entries for 512 series. Both fixed versions emit exactly 512 with
counter total 100, gauge 99, and histogram count 2/sum 100/min 1/max 99.

## Compatibility and failure regressions

| Scenario | Published stack | Revised stack |
|---|---|---|
| Legacy `false`, default StatsD, samples 1 and 99 | Percentile gauges with changed names/types | Two original `latency` distribution observations |
| Same configuration with Prometheus | StatsD percentile gauges; Prometheus sketch | Distribution observations plus complete Prometheus sketch |
| Direct Distribution batch with incompatible sketch | Whole batch rejected | Valid counters/gauges attempted, sketch rejected, error returned |
| `localhost` with an IPv4-only receiver on this dual-stack host | No received packet | Exact `latency:1:99\|d` packet received |
| UDS receiver queue reaches `WouldBlock` | Connection cleared; later sends reconnect | Same socket retained; reports backpressure, resumes after draining |

These five regression tests were copied onto `7618737`, compiled, and failed on
their behavioral assertions; they pass after revision. Additional tests cover all
nine combinations of absent/true/false legacy setting and default/explicit
Percentiles/explicit Distribution mode, incompatible sketches in first/middle/last
position, IPv6-first synthetic resolution, IPv6-only and empty resolution, and
reconnection after a permanent UDS error.

Compatibility is precise: initialization and existing recording signatures stay
valid; unconfigured StatsD plus legacy `false` preserves its metric names/types.
Explicit modes win, so explicit Percentiles plus legacy `false` deliberately emits
percentile gauges. Direct sketch-to-Distribution calls deliberately return errors;
original samples are required. Correct Distribution never substitutes the mean.
See the [migration instructions](../README.md#migrating-the-global-distribution-toggle)
and [release notes](../CHANGELOG.md).

IPv4 preference applies to dual-stack hostnames. Explicit IPv6 and IPv6-only
destinations remain supported. UDP setup cannot determine which receiver is
listening. Failed datagrams are not replayed; errors can mean partial delivery.
UDS backpressure leaves the socket connected; other failures and replacement
allow reconnection. Raw queue rejection counts remain separate from transport errors.

## Export CPU, volume, and paced delivery

The same public-API StatsD Criterion and actual UDP volume/load harnesses were
rerun sequentially on all three versions. Distribution is explicitly configured
for original observations on every version, so means are not compared with samples.

| Workload | Upstream (ms) | Published (ms) | Revised (ms) | Revised vs published |
|---|---:|---:|---:|---:|
| `statsd_export/raw_distribution/1000` | 0.3275 | 0.0748 | 0.0752 | +0.5% |
| `statsd_export/raw_distribution/10` | 0.0042 | 0.0029 | 0.0029 | -0.3% |
| `statsd_export/raw_distribution/100` | 0.0330 | 0.0095 | 0.0094 | -1.1% |
| `statsd_export/percentiles/1000` | 3.8097 | 3.7688 | 3.7859 | +0.5% |
| `statsd_export/percentiles/10` | 0.0357 | 0.0362 | 0.0369 | +2.0% |
| `statsd_export/percentiles/100` | 0.3757 | 0.3738 | 0.3741 | +0.1% |
| `distribution_volume/64_series_96_tag_bytes` | 5.5504 | 0.8554 | 0.9089 | +6.3% |
| `distribution_volume/1_series_8_tag_bytes` | 4.1659 | 0.7318 | 0.7387 | +1.0% |
| `distribution_volume/10000_series_8_tag_bytes` | 4.4347 | 5.0622 | 5.2053 | +2.8% |

The 64-series/96-byte-tag case is 6.3% slower than published in this run; the
unique-series case is 2.8% slower. Packing is not free, especially when each series
has one observation. No new wire-volume reduction is claimed for this revision.

Each finite wire case decodes 10,000 observations, exactly 5,000 values of `1`
and 5,000 values of `99`. All datagrams remain within 1,432 bytes. Payload bytes
exclude UDP/IP headers. Published and revised volume is identical:

| Series / tag bytes | Upstream payload / datagrams | Published and revised payload / datagrams |
|---|---:|---:|
| 1 / 8 | 594,583 / 417 | 26,064 / 19 |
| 64 / 96 | 1,482,318 / 1,112 | 34,302 / 32 |
| 10,000 / 8 | 623,437 / 453 | 623,437 / 453 |

Twelve-second paced workloads use the same default 16,384-slot queue. Every
accepted observation must be decoded from UDP packets, including the shutdown
drain; a receiver mismatch fails the experiment. Callback latency is measured
after UDP export, not as a delivery acknowledgment. Finite runs have no confidence
interval and include all benchmark-process threads in CPU measurements.

| Calls/s | Accepted: upstream / published / revised | Queue drops: upstream / published / revised | p99 callback ms: published → revised |
|---|---:|---:|---:|
| 1,000 | 12,000 / 12,000 / 12,000 | 0 / 0 / 0 | 97.863 → 97.887 |
| 10,000 | 32,768 / 120,000 / 120,000 | 87,232 / 0 / 0 | 97.987 → 97.959 |
| 50,000 | 32,768 / 600,000 / 600,000 | 567,232 / 0 / 0 | 98.505 → 98.534 |

At 50,000 calls/s process CPU is 76.122 ms upstream, 149.873 ms published, and
151.496 ms revised. Revised and published both deliver 600,000 observations;
upstream delivers 32,768. These are different amounts of work, not equivalent-work
speedups. Zero measured loss in these workloads is not a production guarantee.

## Verification

Each implementation PR was also checked in its own archived checkout, with crate
build artifacts cleared between checkouts: PR #7 passes 88 tests, PR #8 passes 97,
and PR #9 passes 113 (all features). Each passes formatting and strict
all-target/all-feature Clippy independently.

Workspace tests pass with default features and all features: 114 passed in each,
with five pre-existing ignored doctests. Formatting, strict all-target/all-feature
Clippy, and strict rustdoc pass. Coverage includes real packets, UTF-8 and newline
byte limits, packing opt-out, overflow, independent flush timers, mixed exporters,
and exactly-once final export of accepted samples after producers join.

```sh
cargo fmt --all -- --check
cargo test --locked --workspace
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
cargo bench --locked --bench name_recording -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench metrics_throughput -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench statsd_export -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench distribution_volume
```

Lossless packing and the scalar recording API are retained. Reservoir sampling,
replacing the exporter package, and native sketch transport remain separate work:
sampling approximates the distribution and can lose rare tails; replacement needs
mixed-exporter integration. No implicit sampling or UDP fallback replay is added.

[Machine-readable results and confidence intervals](stack-review-results.json).
