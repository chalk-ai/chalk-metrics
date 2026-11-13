# Distribution volume and latency

These measurements compare original upstream `96ae2f1dfe728a678556b9be71c80c54b17c2909`,
the first published stack `22ae1f912be5933b735fefe193c4e027b1b387cb`, and the revised
stack containing lossless value packing and independent raw-sample flushing.
They supplement the [original adoption results](adoption-benchmarks.md).

The former default emitted one mean as one distribution observation. It was
smaller because it discarded both sample multiplicity and distribution shape.
All numeric traffic comparisons here instead use original observations on every
version. The upstream client is explicitly configured with the legacy
`aggregate_distr_metrics(false)` setting; built-ins in the revised stack select
raw observations automatically. Correctness is not traded for low traffic.

## Implementation and limits

The revised Distribution exporter groups observations by namespace, metric name,
and tag contents, including interleaved series and separately allocated tags.
It packs values into DogStatsD protocol v1.1 messages, such as
`latency:1:99|d|#service:api`, splitting at the configured UTF-8 byte limit.
Both observations and their multiplicities survive. This reduces repeated
names/tags, not observation count. No sampling or sketch reconstruction occurs.
Agent 6.25+ (6.x) or 7.25+ is required; exporter
`distribution_value_packing(false)` preserves the older wire format.
See [Datadog's protocol](https://docs.datadoghq.com/extend/dogstatsd/datagram_shell/#dogstatsd-protocol-v11).

The worker exports raw observations every 100 milliseconds by default, controlled
by `raw_sample_flush_interval`. Counts, gauges, and sketches remain on their
independent ten-second default interval. A custom `Both` exporter consequently
receives different raw and aggregate windows. Calls remain sequential; slow
exporters can delay both clocks. Raw errors may be logged once per raw batch,
so a shorter interval can increase log frequency during transport failures.

Each queued observation still consumes one slot. Size
`raw_sample_queue_capacity` for peak rate times raw interval, plus bursts and
slow-exporter headroom. Overflow drops newest observations, counts queue
rejections, and leaves mixed-exporter sketches complete. Increasing the queue
alone only postpones overflow. Packing cannot guarantee delivery or bounded
traffic independent of observation rate. Instantaneous bursts above capacity
can still drop samples.

Unique series have no repeated protocol overhead to remove. Grouping adds CPU
and temporary memory; disable packing for workloads dominated by single-sample
series if that cost matters. Local percentile gauges remain available, but
per-process percentiles do not combine into a correct global percentile. A
native mergeable-sketch exporter or explicit sampling would be a separate
approximation/transport design.

## Measurement method

Linux x86_64, AMD EPYC 9B45, 16 logical CPUs, rustc 1.99.0, release profile and
locked dependencies. This shared VM produces noisy timing results; the finite
CPU and latency experiments are single runs, not confidence intervals.

`benches/distribution_volume.rs` uses the production exporter and a concurrently
drained loopback UDP socket. The same source runs on all three revisions using
only upstream APIs. Wire-volume cases send 10,000 observations, exactly 5,000
values of `1` and 5,000 values of `99`, across 1, 64, or 10,000 distinct series.
All packets are decoded, including colon-packed values; a missing observation
or datagram above 1,432 bytes fails the experiment. Bytes below are UDP payload
bytes, excluding IP/UDP headers. Cardinality stays fixed for each comparison.

Paced cases run twelve seconds at 1,000, 10,000, or 50,000 recording calls per
second, in five-millisecond bursts, crossing an aggregate tick and then shutting
down. The queue capacity is 16,384 on every revision. A second custom raw exporter
counts accepted observations and measures time from the recording burst to its
callback after the UDP export. Queue drops are recorded minus accepted; decoded
UDP observations must equal accepted, distinguishing transport loss from queue
overflow. Latency describes accepted observations, not receiver acknowledgment.
CPU includes producers, worker, instrumentation, and the UDP receiver thread.
Cases with different accepted counts perform different amounts of work.

Criterion cases in `benches/statsd_export.rs` use the same three cardinality/tag
workloads with 10,000 values per iteration. They measure formatting and send
attempts with persistent draining receivers, with one-second warmup, two-second
measurement, and twenty samples. They do not claim acknowledged delivery for
each timed iteration; the separate finite experiment checks complete receipt.

## Results

All wire-volume cases delivered exactly 10,000 observations, including 5,000 each of `1` and `99`, on every revision. Upstream and the published stack have identical payload and datagram counts.

| Series / endpoint tag bytes | Payload bytes before → after | Reduction | Datagrams before → after |
|---|---:|---:|---:|
| 1 / 8 | 594,583 → 26,064 | 95.6% | 417 → 19 |
| 64 / 96 | 1,482,318 → 34,302 | 97.7% | 1,112 → 32 |
| 10,000 / 8 | 623,437 → 623,437 | 0.0% | 453 → 453 |

Criterion formatting/send point estimates per 10,000 observations (slope when available):

| Series / tag bytes | Upstream (ms) | Published stack (ms) | Revised (ms) | Change vs published |
|---|---:|---:|---:|---:|
| 1 / 8 | 4.164 | 4.233 | 0.752 | -82.2% |
| 64 / 96 | 6.156 | 5.492 | 0.862 | -84.3% |
| 10,000 / 8 | 4.361 | 4.666 | 5.061 | +8.5% |

Packing has **no byte benefit and an 8.5% timing cost versus the published stack** in the 10,000-distinct-series case (16.1% versus upstream). Disable it for sparse-series workloads if appropriate. Repeated-series improvements do not imply a universal speedup.

Twelve-second paced runs, with the default queue capacity and a ten-second aggregate interval:

| Calls/s | Recorded | Accepted before → after | Queue drops before → after | p99 callback latency before → after (ms) |
|---|---:|---:|---:|---:|
| 1,000 | 12,000 | 12,000 → 12,000 | 0 → 0 | 9,886.6 → 97.9 |
| 10,000 | 120,000 | 32,768 → 120,000 | 87,232 → 0 | 9,983.0 → 97.9 |
| 50,000 | 600,000 | 32,768 → 600,000 | 567,232 → 0 | 10,006.5 → 98.5 |

Every accepted observation was received over UDP on all three revisions, including final shutdown. Upstream has the same accepted/drop counts as the published stack. A zero-drop finite run is not a guarantee for other rates, bursts, slow exporters, or production networks.

| Calls/s | Process CPU before → after (ms) | Payload bytes before → after | Datagrams before → after |
|---|---:|---:|---:|
| 1,000 | 21.004 → 21.911 | 744,364 → 67,610 | 526 → 120 |
| 10,000 | 47.072 → 44.831 | 4,949,600 → 816,730 | 3,642 → 610 |
| 50,000 | 61.271 → 151.242 | 4,977,248 → 5,205,130 | 3,642 → 3,840 |

At 50,000 calls/s the revised worker exports 600,000 observations instead of 32,768: **18.3 times as many accepted observations**. Total CPU and payload can consequently rise despite much lower overhead per observation. These paced cases are not equivalent-work speedup comparisons.

The accepted-raw recording benchmark reports 45.630 µs per 1,000 calls. Startup/shutdown are excluded; all calls fit in a fresh queue. Historic upstream and first-stack estimates remain in the original adoption report.

## Reproduction and validation

```sh
cargo bench --locked --bench distribution_volume
cargo bench --locked --bench statsd_export -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench metrics_throughput -- raw_accepted --warm-up-time 1 --measurement-time 2 --sample-size 20
```

Run sequentially. For each baseline checkout, copy both unchanged benchmark
sources from this revision into `benches/` and add the `distribution_volume`
`[[bench]]` target with `harness = false` to its manifest. No production baseline
code is modified. The finite experiment writes JSON to stdout; Criterion writes
estimates to `target/criterion`. Machine-readable measurements are preserved in
[distribution-volume-results.json](distribution-volume-results.json).

Regression tests cover both runtime configurations, queue recycling before
aggregate flushes, complete mixed-exporter sketches, deterministic overflow,
packed UTF-8 byte boundaries, interleaved tags/namespaces, the legacy wire-format
opt-out, failed-datagram continuation without replay, and final shutdown delivery.
