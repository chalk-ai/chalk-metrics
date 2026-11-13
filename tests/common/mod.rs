/// Decode protocol v1.1 value packing so assertions count observations, not lines.
pub fn expand_lines(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| {
            let (head, suffix) = line.split_once('|').unwrap();
            if suffix.split('|').next() != Some("d") {
                return vec![line.to_owned()];
            }
            let (name, values) = head.split_once(':').unwrap();
            values
                .split(':')
                .map(|value| format!("{name}:{value}|{suffix}"))
                .collect()
        })
        .collect()
}
