# Goalpost benchmarks

These benches track costs the earlier ones did not reach: the generated
recording API with real tags, thread scaling, flush, delivery to a local
receiver, export cost by metric kind, wire size, and macro compile time. The
values below are what the library does today, recorded so that later changes can
be measured against them. They are not targets, and none of the timings gate CI.
The one enforced number is allocations per recording call
(`tests/alloc_budget.rs`, which runs under `cargo test`).

## Running

```sh
cargo bench --locked --bench generated_recording -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench thread_scaling -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench flush -- --warm-up-time 1 --measurement-time 2 --sample-size 20
cargo bench --locked --bench statsd_export -- by_kind --warm-up-time 1 --measurement-time 2
cargo bench --locked --bench prometheus_export -- by_kind --warm-up-time 1 --measurement-time 2
cargo bench --locked --bench delivery
cargo bench --locked --bench compile_time
cargo test --locked --test alloc_budget
```

All of them together take about five minutes. `delivery` and `compile_time` are
finite runs that print JSON rows; `flush` prints two JSON lines after its
Criterion groups. Each bench file's header describes its cases.

## Environment for the values below

Linux x86_64, AMD EPYC 9B45 (8 cores, 16 logical CPUs), rustc 1.99.1, release
profile, locked dependencies, 2026-11-14. This is a shared virtual machine and
each bench ran once, so treat timing differences under about 10% as noise and
compare only runs from the same machine. Socket settings: `net.core.rmem_default`
and `net.core.wmem_default` 212992, `net.unix.max_dgram_qlen` 512.

## Recording through generated metrics

`generated_recording` (time per call) and `tests/alloc_budget.rs` (exact
allocations per call). "Built" constructs the metric struct, including its
`String` tags, on every call; "prebuilt" reuses one struct.

| Case | ns per call | Allocations per call |
|---|---:|---:|
| Client not initialized, count, no tags | 2.3 | 0 |
| Client not initialized, count, string + enum built | 15.4 | 1 |
| Count, no tags | 16.4 | 0 |
| Count, one enum | 20.1 | 0 |
| Count, two enums | 21.8 | 0 |
| Count, string + enum prebuilt | 23.7 | 0 |
| Count, string + enum built | 42.0 | 1 |
| Count, 57-byte string + enum built | 46.0 | not tracked |
| Count, five tags prebuilt | 43.0 | 0 |
| Count, five tags built (three strings) | 65.2 | 3 |
| Gauge, string + enum built | 39.8 | 1 |
| Histogram (sketch), no tags | 53.0 | 0 |
| Histogram (sketch), string + enum built | 97.6 | 1 |
| Histogram (sketch), five tags built | 121.9 | not tracked |

## Thread scaling

`thread_scaling`: total recording calls per second across all threads, in
millions. Perfect scaling multiplies the one-thread figure by the thread count.

| Scenario | 1 thread | 2 | 4 | 8 | 16 |
|---|---:|---:|---:|---:|---:|
| Count, one series shared by all threads | 59.9 | 39.8 | 20.2 | 6.9 | 2.5 |
| Count, one series and stripe per thread | 59.9 | 119.8 | 239.4 | 475.1 | 818.0 |
| Count, 80-series pool | 60.8 | 57.5 | 71.2 | 82.3 | 46.1 |
| Histogram into a sketch, one series | 18.0 | 11.0 | 6.8 | 3.1 | 2.2 |
| Histogram as raw samples, one series | 10.3 | 5.5 | 5.3 | 3.7 | 2.5 |
| Histogram as raw samples, one series per thread | 10.1 | 7.1 | 7.0 | 6.9 | 5.8 |

## Flush

`flush`: time for one `flush()` call, per series. Histograms hold 20
observations each.

| Kind | 1,000 series | 10,000 | 100,000 |
|---|---:|---:|---:|
| Count | 22 ns | 23 ns | 35 ns |
| Gauge | 12 ns | 12 ns | 19 ns |
| Histogram | 97 ns | 157 ns | 406 ns |

Recording 10,000 existing count series once costs 16 ns per series; the same
pass right after a flush costs 88 ns per series, because flush removes count and
histogram entries and each series is registered again.

Recording latency seen by one thread while another flushes populated histogram
series:

| Histogram series | Flush time | Max without a flush | p99.9 during the flush | Max during the flush |
|---|---:|---:|---:|---:|
| 10,000 | 4.1 ms | 13 µs | 54 µs | 274 µs |
| 100,000 | 40.5 ms | 8 µs | 595 µs | 885 µs |

## Delivery

`delivery`, section `raw_ceiling`: a default client (16,384-slot raw-sample
queue drained every 100 ms) in Distribution mode, paced for two seconds. Queue
drops are a share of what was recorded; transport loss is a share of what the
queue accepted. Accepted samples cap at 163,840 per second on both transports.

| Target observations/s | UDP queue drops | UDP transport loss | UDS queue drops | UDS transport loss |
|---:|---:|---:|---:|---:|
| 50,000 | 0% | 0% | 0% | 0% |
| 100,000 | 0% | 0% | 0% | 8.1% |
| 150,000 | 0% | 1.7% | 0% | 45.3% |
| 200,000 | 18.1% | 6.8% | 18.1% | 47.1% |
| 400,000 | 59.0% | 9.2% | 59.0% | 45.4% |

Section `aggregate_flush`: counters and gauges passed to `Exporter::export` five
times. UDP delivered every line at 1,000 and 10,000 series, and so did the Unix
socket at 1,000. At 10,000 series over the Unix socket, 3,903 to 8,325 of
10,000 lines arrived (median 7,856) and all five calls returned a send error.

Delivery loss varies from run to run, more than the timings do. An earlier run
on the same machine under heavier load delivered 3,477 of 10,000 lines in one
UDP flush with no error returned.

Section `wire_bytes`: payload bytes per raw observation, 10,000 observations on
one series with value packing.

| Value shape | Bytes per observation |
|---|---:|
| Alternating `1` and `99` (as in `distribution_volume`) | 2.6 |
| Latency in seconds, full `f64` precision | 21.3 |
| Latency in seconds, four significant digits | 8.3 |
| Latency in whole microseconds | 6.0 |

## Export cost by metric kind

The `by_kind` groups in `statsd_export` and `prometheus_export`: 10,000 series
of one kind per export, time per series. Histogram sketches hold 100
observations each; StatsD uses Percentiles mode over UDP.

| Kind | StatsD export | Prometheus render |
|---|---:|---:|
| Count | 0.32 µs | 0.25 µs |
| Gauge | 0.28 µs | 0.35 µs |
| Histogram | 12.3 µs | 16.4 µs |

## Macro compile time

`compile_time`: time to rebuild a crate whose only content is one
`define_metrics!` group, with dependencies already built.

| Metrics defined | Dev build | Release build |
|---:|---:|---:|
| 100 | 0.4 s | 1.5 s |
| 1,000 | 3.5 s | 15.2 s |
