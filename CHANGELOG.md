# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.4](https://github.com/chalk-ai/chalk-metrics/compare/v0.0.0...v0.0.4) - 2026-11-14

### Security

- Fix RUSTSEC-2026-0386 (tokio) and, with `python-bindings`, RUSTSEC-2026-0176/0177 (pyo3).

### Added

- IPv6 StatsD destinations.
- Bounded raw-sample queue and a separate raw flush interval.

### Changed

- Distribution exporters receive every observation instead of a sketch mean;
  values are packed by default (Datadog Agent 6.25+/7.25+).
- Explicit StatsD histogram modes override the deprecated `aggregate_distr_metrics` toggle.
- Exporting sketches as Distribution returns an error; use `export_raw_points`.
- `python-bindings` requires pyo3 0.30. MSRV is 1.88. tokio's `net` feature is no
  longer enabled.
- `chalk-metrics-macros` is now published alongside `chalk-metrics`.

### Fixed

- Series lookup after aggregation-map growth no longer splits or drops data.
- UDS connections survive backpressure and reconnect after failures.
