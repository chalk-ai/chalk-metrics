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
