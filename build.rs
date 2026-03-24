#[path = "src/schema.rs"]
mod schema;
#[path = "src/codegen.rs"]
mod codegen;

fn main() {
    codegen::generate_with_crate_path("metrics.json", "crate");
}
