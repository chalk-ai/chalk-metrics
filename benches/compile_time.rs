//! Build time of a crate that defines many metrics with the macros.
//!
//! Not a Criterion bench: it writes a throwaway library crate under the target
//! directory, builds it with the cargo that is running this bench, and prints
//! one JSON row per metric count and profile. Dependencies are compiled once,
//! untimed, so each row is the time to rebuild only the crate holding the
//! `define_metrics!` invocation. The one-metric row is the fixed cost of such a
//! rebuild (cargo start-up, the tag definitions, linking), so the cost of the
//! metrics themselves is the difference between rows. The fixture reuses this
//! repository's lockfile and builds offline, so it needs no network once the
//! repository itself builds.
//!
//! Runs only under `cargo bench` (which passes `--bench`), and only when no
//! name filter excludes `compile_time`, so `cargo test --benches` and
//! `cargo bench -- <some other bench>` do not start builds.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const SIZES: [usize; 3] = [1, 100, 1_000];

fn main() {
    if !selected("compile_time") {
        eprintln!("compile_time: skipped (needs `--bench` and no filter excluding it)");
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = target_dir().join("compile-time-fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    std::fs::write(
        fixture.join("Cargo.toml"),
        format!(
            "[package]\nname = \"compile-time-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
             [dependencies]\nchalk-metrics = {{ path = {repo:?} }}\n\n\
             # Standalone: not a member of the repository workspace.\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::copy(repo.join("Cargo.lock"), fixture.join("Cargo.lock")).unwrap();

    // Compile dependencies for both profiles before timing anything.
    write_metrics(&fixture, 1);
    build(&fixture, false);
    build(&fixture, true);

    let mut rows = Vec::new();
    for metrics in SIZES {
        for release in [false, true] {
            // Rewriting the source forces a rebuild of the fixture crate only.
            write_metrics(&fixture, metrics);
            let start = Instant::now();
            build(&fixture, release);
            rows.push(format!(
                "{{\"workload\":\"compile_time\",\"metrics\":{metrics},\"profile\":\"{}\",\"seconds\":{:.2}}}",
                if release { "release" } else { "dev" },
                start.elapsed().as_secs_f64()
            ));
        }
    }
    println!("[\n{}\n]", rows.join(",\n"));
}

/// Whether this finite run was asked for. `cargo bench` passes `--bench` and
/// `cargo test --benches` does not; a positional argument is a name filter, as
/// in Criterion; `--list` only lists.
fn selected(name: &str) -> bool {
    let mut bench = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--bench" => bench = true,
            "--list" => return false,
            filter if !filter.starts_with('-') && !name.contains(filter) => return false,
            _ => {}
        }
    }
    bench
}

/// The bench executable lives in `<target>/<profile>/deps/`.
fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .nth(3)
        .expect("bench executable is inside a cargo target directory")
        .to_path_buf()
}

/// Counts, gauges and histograms in rotation, each with one enum tag and two
/// string tags, in a single `define_metrics!` group.
fn write_metrics(fixture: &Path, metrics: usize) {
    let mut source = String::from(
        "chalk_metrics::define_tags! {\n    pub Status => \"status\" {\n        Success => \"success\",\n        Failure => \"failure\",\n    }\n    pub Endpoint => \"endpoint\";\n    pub Region => \"region\";\n}\n\n\
         chalk_metrics::define_metrics! {\n    group(tags = [Endpoint, Status, Region]) {\n",
    );
    for index in 0..metrics {
        let kind = ["count", "gauge", "histogram"][index % 3];
        source.push_str(&format!(
            "        pub {kind} M{index} => \"metric_{index}\", \"generated metric {index}\";\n"
        ));
    }
    source.push_str("    }\n}\n");
    std::fs::write(fixture.join("src/lib.rs"), source).unwrap();
}

fn build(fixture: &Path, release: bool) {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(fixture)
        .args(["build", "--offline", "--quiet", "--target-dir"])
        .arg(fixture.join("target"));
    if release {
        command.arg("--release");
    }
    let output = command.output().expect("failed to run cargo");
    assert!(
        output.status.success(),
        "fixture build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
