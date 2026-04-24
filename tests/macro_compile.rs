#[test]
fn macro_compile_tests() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/macro_valid.rs");
    t.compile_fail("tests/ui/macro_invalid_metric_type.rs");
}
