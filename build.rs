#[path = "src/schema.rs"]
mod schema;
#[path = "src/codegen.rs"]
mod codegen;

fn main() {
    codegen::generate("metrics.json");
}
