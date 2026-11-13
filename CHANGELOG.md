# Release notes

## Unreleased

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
