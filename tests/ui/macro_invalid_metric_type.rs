chalk_metrics::define_metrics! {
    group(tags = []) {
        pub timer BadMetric => "bad_metric", "Bad metric";
    }
}

fn main() {}
