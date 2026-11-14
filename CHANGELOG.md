# Release notes

## Unreleased

- Build and dependencies: update the lockfile (fixes RUSTSEC-2026-0386 in tokio and
  RUSTSEC-2026-0204 in a dev dependency), drop tokio's unused `net` feature, move to
  hashbrown 0.17 and syn 3, and declare `rust-version = "1.88"`. Exporters that use
  `tokio::net` must enable tokio's `net` feature themselves.
- `python-bindings` now uses pyo3 0.30 (fixes RUSTSEC-2026-0176 and RUSTSEC-2026-0177).
  Crates enabling it must depend on pyo3 0.30. Generated tag classes opt into
  `from_py_object` so metric constructors still accept them.
- Releases: `make release` creates a GitHub release, and publishing it publishes both
  crates with the version taken from the tag.
- Packaging: `chalk-metrics-macros` is versioned with `chalk-metrics` and published
  first, and the crate tarball contains only sources, README, LICENSE and CHANGELOG.
- Fix series lookup after aggregation-map growth. A series now produces one entry
  with complete counter totals, latest gauge updates, and all histogram samples.
  Generated metrics compute content-based name fingerprints at compile time;
  existing string recording APIs remain available and identify the same series.
- Route histogram input per exporter. Distribution receives original observations,
  while Prometheus and StatsD Percentiles receive sketches. Mixed exporters retain
  both without duplicating observations within a destination.
- Preserve legacy `aggregate_distr_metrics(false)` with default, unconfigured StatsD
  as Distribution output. Before removing the deprecated toggle, explicitly select
  `HistogramExportMode::Distribution`. Existing initialization signatures remain valid.
- Explicit StatsD modes take precedence over the toggle. Explicit Percentiles plus
  legacy `false` previously emitted `name:v|d`; it now emits `name.count`, `name.avg`,
  and percentile gauges. Select Distribution to retain the old names and types.
- Distribution no longer converts a sketch's mean into one observation: samples
  `1` and `99` remain two observations. Lossless value packing is enabled by default
  and requires Agent 6.25+ (6.x) or 7.25+; `distribution_value_packing(false)` retains
  separate-value lines. No sampling is enabled.
- Direct Distribution export of sketches returns an error. Use `export_raw_points`
  with original observations. Valid counters and gauges in a rejected batch are
  still sent; a batch error can indicate partial delivery.
- Support IPv4 and IPv6 destinations, preferring IPv4 for dual-stack hostnames
  such as `localhost`. UDP setup cannot verify that an agent is listening.
- Retain UDS connections on receiver backpressure (`WouldBlock`) and reconnect
  after other failures or socket replacement. Failed datagrams are not retried.
- Reject oversized individual lines while attempting valid siblings. Expose
  bounded raw-queue capacity and an independent raw flush interval (100 ms default).
  Overflow drops newest raw observations and counts queue rejections; mixed
  sketches stay complete. Stop/join producers and explicitly shut down local
  clients to flush accepted observations; shutdown is best effort, not acknowledgment.
