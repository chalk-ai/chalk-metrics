//! The production sketch must behave exactly like the original implementation
//! (kept verbatim in `common/sketch_reference.rs`): same buckets in the same
//! order, same statistics, same quantile and CDF estimates, bit for bit.

#[path = "common/sketch_reference.rs"]
mod reference;

use chalk_metrics::export::UDDSketch;
use reference::UDDSketch as Reference;

const QUANTILES: [f64; 8] = [0.0, 0.1, 0.5, 0.9, 0.95, 0.99, 0.999, 1.0];
const BOUNDARIES: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
const VALUES_PER_CASE: usize = 24_000;
/// (max_buckets, initial_error): roomy (rarely compacts) and tight (compacts a lot).
const CONFIGS: [(u64, f64); 2] = [(200, 0.001), (48, 0.01)];

/// Deterministic xorshift64* generator.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in (0, 1).
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

fn distributions(seed: u64) -> Vec<(&'static str, Vec<f64>)> {
    let mut rng = Rng(seed);
    let mut sample = |f: &mut dyn FnMut(&mut Rng) -> f64| -> Vec<f64> {
        (0..VALUES_PER_CASE).map(|_| f(&mut rng)).collect()
    };
    vec![
        ("constant", sample(&mut |_| 0.25)),
        ("lognormal", sample(&mut |r| r.normal().exp())),
        ("heavy_lognormal", sample(&mut |r| (2.5 * r.normal()).exp())),
        (
            "uniform_three_decades",
            sample(&mut |r| 10f64.powf(r.uniform() * 3.0 - 2.0)),
        ),
        (
            "pareto",
            sample(&mut |r| 0.01 / r.uniform().powf(1.0 / 1.2)),
        ),
        (
            "bimodal",
            sample(&mut |r| {
                let mode = if r.uniform() < 0.7 { 0.05 } else { 5.0 };
                mode * (0.2 * r.normal()).exp()
            }),
        ),
    ]
}

fn build(values: &[f64], (max_buckets, error): (u64, f64)) -> (UDDSketch, Reference) {
    let mut new = UDDSketch::new(max_buckets, error);
    let mut old = Reference::new(max_buckets, error);
    for &v in values {
        new.add_value(v);
        old.add_value(v);
    }
    (new, old)
}

fn bits(values: &[f64]) -> Vec<u64> {
    values.iter().map(|v| v.to_bits()).collect()
}

fn assert_equivalent(label: &str, new: &UDDSketch, old: &Reference, boundaries: &[f64]) {
    assert_eq!(new.count(), old.count(), "{label}: count");
    assert_eq!(new.sum().to_bits(), old.sum().to_bits(), "{label}: sum");
    assert_eq!(new.min().to_bits(), old.min().to_bits(), "{label}: min");
    assert_eq!(new.max().to_bits(), old.max().to_bits(), "{label}: max");
    assert_eq!(new.zero_count(), old.zero_count(), "{label}: zero_count");
    assert_eq!(
        new.max_error().to_bits(),
        old.max_error().to_bits(),
        "{label}: max_error"
    );
    assert_eq!(
        new.current_buckets_count(),
        old.current_buckets_count(),
        "{label}: bucket count"
    );

    let new_buckets: Vec<String> = new.bucket_iter().map(|b| format!("{b:?}")).collect();
    let old_buckets: Vec<String> = old.bucket_iter().map(|b| format!("{b:?}")).collect();
    assert_eq!(new_buckets, old_buckets, "{label}: buckets in order");

    let expected_q: Vec<f64> = QUANTILES
        .iter()
        .map(|&q| old.estimate_quantile(q))
        .collect();
    for (&q, &want) in QUANTILES.iter().zip(&expected_q) {
        assert_eq!(
            new.estimate_quantile(q).to_bits(),
            want.to_bits(),
            "{label}: estimate_quantile({q})"
        );
    }
    let mut got = [0.0; QUANTILES.len()];
    new.estimate_quantiles(&QUANTILES, &mut got);
    assert_eq!(bits(&got), bits(&expected_q), "{label}: estimate_quantiles");

    // Non-ascending order, and a repeated quantile, must give the same answers.
    let shuffled = [0.99, 0.0, 0.5, 0.5, 1.0, 0.1, 0.999, 0.9];
    let expected: Vec<f64> = shuffled.iter().map(|&q| old.estimate_quantile(q)).collect();
    let mut got = [0.0; 8];
    new.estimate_quantiles(&shuffled, &mut got);
    assert_eq!(bits(&got), bits(&expected), "{label}: shuffled quantiles");

    let expected_c: Vec<f64> = boundaries
        .iter()
        .map(|&v| old.estimate_quantile_at_value(v))
        .collect();
    for (&v, &want) in boundaries.iter().zip(&expected_c) {
        assert_eq!(
            new.estimate_quantile_at_value(v).to_bits(),
            want.to_bits(),
            "{label}: estimate_quantile_at_value({v})"
        );
    }
    let mut got = vec![0.0; boundaries.len()];
    new.estimate_quantiles_at_values(boundaries, &mut got);
    assert_eq!(
        bits(&got),
        bits(&expected_c),
        "{label}: estimate_quantiles_at_values"
    );

    let reversed: Vec<f64> = boundaries.iter().rev().copied().collect();
    let expected: Vec<f64> = reversed
        .iter()
        .map(|&v| old.estimate_quantile_at_value(v))
        .collect();
    let mut got = vec![0.0; reversed.len()];
    new.estimate_quantiles_at_values(&reversed, &mut got);
    assert_eq!(
        bits(&got),
        bits(&expected),
        "{label}: reversed estimate_quantiles_at_values"
    );
}

#[test]
fn matches_reference_for_every_distribution() {
    for config in CONFIGS {
        for (name, values) in distributions(0x9E37_79B9_7F4A_7C15) {
            let (new, old) = build(&values, config);
            assert_equivalent(&format!("{name} {config:?}"), &new, &old, &BOUNDARIES);
        }
    }
}

#[test]
fn matches_reference_after_merging() {
    for config in CONFIGS {
        let first = distributions(0x1234_5678_9ABC_DEF1);
        let second = distributions(0x0FED_CBA9_8765_4321);
        for ((name, a), (_, b)) in first.iter().zip(&second) {
            let (mut new_a, mut old_a) = build(a, config);
            let (new_b, old_b) = build(b, config);
            new_a.merge_sketch(&new_b);
            old_a.merge_sketch(&old_b);
            assert_equivalent(
                &format!("{name} merged {config:?}"),
                &new_a,
                &old_a,
                &BOUNDARIES,
            );
        }

        // Uneven compaction levels: a wide sketch merged with a narrow one, both ways.
        let wide = &first[3].1;
        let narrow = &second[0].1[..50];
        let (mut new_a, mut old_a) = build(wide, config);
        let (new_b, old_b) = build(narrow, config);
        new_a.merge_sketch(&new_b);
        old_a.merge_sketch(&old_b);
        assert_equivalent("uneven merge", &new_a, &old_a, &BOUNDARIES);
        let (mut new_c, mut old_c) = build(narrow, config);
        let (new_d, old_d) = build(wide, config);
        new_c.merge_sketch(&new_d);
        old_c.merge_sketch(&old_d);
        assert_equivalent("uneven merge reversed", &new_c, &old_c, &BOUNDARIES);
    }
}

#[test]
fn matches_reference_with_zeros_negatives_and_empty() {
    let mut rng = Rng(42);
    let values: Vec<f64> = (0..VALUES_PER_CASE)
        .map(|i| match i % 5 {
            0 => 0.0,
            1 | 2 => -(rng.normal() * 3.0).exp(),
            _ => (rng.normal() * 3.0).exp(),
        })
        .collect();
    let boundaries = [-10.0, -1.0, -0.01, 0.0, 0.01, 1.0, 10.0, 100.0];
    for config in CONFIGS {
        let (new, old) = build(&values, config);
        assert_equivalent("mixed signs", &new, &old, &boundaries);

        let (new, old) = build(&[], config);
        let mut got = [1.0; 8];
        new.estimate_quantiles(&QUANTILES, &mut got);
        assert_eq!(
            bits(&got),
            bits(&QUANTILES.map(|q| old.estimate_quantile(q)))
        );
        let mut got = [1.0; 8];
        new.estimate_quantiles_at_values(&boundaries, &mut got);
        assert_eq!(
            bits(&got),
            bits(&boundaries.map(|v| old.estimate_quantile_at_value(v)))
        );

        let (new, old) = build(&[3.0], config);
        assert_equivalent("single value", &new, &old, &BOUNDARIES);
        new.estimate_quantiles(&[], &mut []);
        new.estimate_quantiles_at_values(&[], &mut []);
    }
}
