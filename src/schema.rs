use serde::Deserialize;
use std::collections::HashMap;

/// Top-level metrics schema parsed from the JSON definition file.
#[derive(Debug, Deserialize)]
pub struct MetricsSchema {
    pub tags: HashMap<String, TagDefinition>,
    /// Top-level metrics (no namespace).
    #[serde(default)]
    pub metrics: Vec<MetricDefinition>,
    /// Nested namespace blocks containing metrics and further namespaces.
    #[serde(default)]
    pub namespaces: HashMap<String, NamespaceDefinition>,
}

/// A namespace block that can contain metrics and further nested namespaces.
#[derive(Debug, Deserialize)]
pub struct NamespaceDefinition {
    #[serde(default)]
    pub metrics: Vec<MetricDefinition>,
    #[serde(default)]
    pub namespaces: HashMap<String, NamespaceDefinition>,
}

/// Defines a reusable tag with value constraints and a default export name.
#[derive(Debug, Deserialize)]
pub struct TagDefinition {
    pub value_type: TagValueType,
    /// Required when `value_type` is `Enum`. Lists the allowed string values.
    pub values: Option<Vec<String>>,
    /// The default key name used when exporting this tag.
    pub export_name: String,
}

/// Whether a tag's values are constrained to a fixed enum or accept arbitrary strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagValueType {
    Enum,
    String,
}

/// A metric definition: name, aggregation type, associated tags, and description.
#[derive(Debug, Deserialize)]
pub struct MetricDefinition {
    pub name: String,
    #[serde(rename = "type")]
    pub metric_type: MetricType,
    pub tags: Vec<MetricTagRef>,
    pub description: String,
}

/// The aggregation type for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricType {
    Count,
    Gauge,
    Histogram,
}

/// A reference from a metric to a globally defined tag, with optional overrides.
#[derive(Debug, Deserialize)]
pub struct MetricTagRef {
    /// The name of the tag definition in the top-level `tags` map.
    pub tag: String,
    /// Override the export name for this tag on this specific metric.
    pub export_name: Option<String>,
    /// Whether this tag is optional for this metric. Defaults to `false`.
    #[serde(default)]
    pub optional: bool,
}

/// Validation errors found in a metrics schema.
#[derive(Debug)]
pub struct ValidationError {
    pub errors: Vec<String>,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, err) in self.errors.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "- {err}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

/// A flattened metric with its namespace path.
pub struct FlatMetric<'a> {
    pub namespace: Vec<String>,
    pub metric: &'a MetricDefinition,
}

impl MetricsSchema {
    /// Parse a metrics schema from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Recursively flatten all metrics from the namespace tree.
    /// Returns `(namespace_path, metric)` pairs in a deterministic order:
    /// top-level metrics first, then namespaces in sorted order (depth-first).
    pub fn flatten_metrics(&self) -> Vec<FlatMetric<'_>> {
        let mut result = Vec::new();
        // Top-level metrics
        for metric in &self.metrics {
            result.push(FlatMetric {
                namespace: vec![],
                metric,
            });
        }
        // Recurse into namespaces (sorted for deterministic output)
        let mut ns_names: Vec<&String> = self.namespaces.keys().collect();
        ns_names.sort();
        for ns_name in ns_names {
            let ns_def = &self.namespaces[ns_name];
            flatten_namespace(ns_def, &[ns_name.to_string()], &mut result);
        }
        result
    }

    /// Validate the schema for internal consistency. Returns `Ok(())` if valid,
    /// or a `ValidationError` containing all problems found.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = Vec::new();

        // Check enum tags have at least one value
        for (tag_name, tag_def) in &self.tags {
            if tag_def.value_type == TagValueType::Enum {
                match &tag_def.values {
                    None => {
                        errors.push(format!(
                            "tag '{tag_name}': enum tag must have a 'values' field"
                        ));
                    }
                    Some(values) if values.is_empty() => {
                        errors.push(format!(
                            "tag '{tag_name}': enum tag must have at least one value"
                        ));
                    }
                    _ => {}
                }
            }
        }

        // Flatten all metrics and check for duplicates + tag refs
        let flat = self.flatten_metrics();

        // Check for duplicate qualified names (namespace + name)
        let mut seen_qualified = HashMap::new();
        for (i, fm) in flat.iter().enumerate() {
            let qualified = if fm.namespace.is_empty() {
                fm.metric.name.clone()
            } else {
                format!("{}::{}", fm.namespace.join("::"), fm.metric.name)
            };
            if let Some(prev_idx) = seen_qualified.insert(qualified.clone(), i) {
                errors.push(format!(
                    "duplicate metric '{}' (indices {prev_idx} and {i})",
                    qualified
                ));
            }
        }

        // Check metric tag references point to defined tags
        for fm in &flat {
            let qualified = if fm.namespace.is_empty() {
                fm.metric.name.clone()
            } else {
                format!("{}::{}", fm.namespace.join("::"), fm.metric.name)
            };
            for tag_ref in &fm.metric.tags {
                if !self.tags.contains_key(&tag_ref.tag) {
                    errors.push(format!(
                        "metric '{qualified}': references undefined tag '{}'",
                        tag_ref.tag
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { errors })
        }
    }
}

fn flatten_namespace<'a>(
    ns: &'a NamespaceDefinition,
    path: &[String],
    result: &mut Vec<FlatMetric<'a>>,
) {
    for metric in &ns.metrics {
        result.push(FlatMetric {
            namespace: path.to_vec(),
            metric,
        });
    }
    let mut child_names: Vec<&String> = ns.namespaces.keys().collect();
    child_names.sort();
    for child_name in child_names {
        let child_ns = &ns.namespaces[child_name];
        let mut child_path = path.to_vec();
        child_path.push(child_name.clone());
        flatten_namespace(child_ns, &child_path, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_schema() {
        let json = r#"{"tags": {}}"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        assert!(schema.tags.is_empty());
        assert!(schema.metrics.is_empty());
        assert!(schema.namespaces.is_empty());
        schema.validate().unwrap();
    }

    #[test]
    fn test_parse_full_example() {
        let json = r#"{
            "tags": {
                "status": {
                    "value_type": "enum",
                    "values": ["success", "failure", "timeout"],
                    "export_name": "status"
                },
                "endpoint": {
                    "value_type": "string",
                    "export_name": "endpoint"
                }
            },
            "metrics": [
                {
                    "name": "request_latency",
                    "type": "histogram",
                    "tags": [
                        { "tag": "endpoint" },
                        { "tag": "status" }
                    ],
                    "description": "HTTP request latency"
                }
            ]
        }"#;

        let schema = MetricsSchema::from_json(json).unwrap();
        assert_eq!(schema.tags.len(), 2);
        assert_eq!(schema.metrics.len(), 1);
        schema.validate().unwrap();
    }

    #[test]
    fn test_parse_with_namespaces() {
        let json = r#"{
            "tags": {
                "status": { "value_type": "enum", "values": ["ok"], "export_name": "status" }
            },
            "metrics": [
                { "name": "uptime", "type": "gauge", "tags": [], "description": "top-level" }
            ],
            "namespaces": {
                "http": {
                    "metrics": [
                        { "name": "request_count", "type": "count", "tags": [{"tag": "status"}], "description": "HTTP requests" }
                    ],
                    "namespaces": {
                        "auth": {
                            "metrics": [
                                { "name": "login_latency", "type": "histogram", "tags": [], "description": "Login latency" }
                            ]
                        }
                    }
                }
            }
        }"#;

        let schema = MetricsSchema::from_json(json).unwrap();
        schema.validate().unwrap();

        let flat = schema.flatten_metrics();
        assert_eq!(flat.len(), 3);

        // Top-level
        assert!(flat[0].namespace.is_empty());
        assert_eq!(flat[0].metric.name, "uptime");

        // http namespace
        assert_eq!(flat[1].namespace, vec!["http"]);
        assert_eq!(flat[1].metric.name, "request_count");

        // http::auth namespace
        assert_eq!(flat[2].namespace, vec!["http", "auth"]);
        assert_eq!(flat[2].metric.name, "login_latency");
    }

    #[test]
    fn test_parse_count_and_gauge_types() {
        let json = r#"{
            "tags": {},
            "metrics": [
                { "name": "req_count", "type": "count", "tags": [], "description": "count" },
                { "name": "temperature", "type": "gauge", "tags": [], "description": "gauge" }
            ]
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        assert_eq!(schema.metrics[0].metric_type, MetricType::Count);
        assert_eq!(schema.metrics[1].metric_type, MetricType::Gauge);
    }

    #[test]
    fn test_validate_missing_tag_ref_in_namespace() {
        let json = r#"{
            "tags": {},
            "namespaces": {
                "ns": {
                    "metrics": [
                        { "name": "m1", "type": "count", "tags": [{ "tag": "nonexistent" }], "description": "test" }
                    ]
                }
            }
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("ns::m1"));
        assert!(err.errors[0].contains("undefined tag 'nonexistent'"));
    }

    #[test]
    fn test_validate_duplicate_metric_names_same_namespace() {
        let json = r#"{
            "tags": {},
            "namespaces": {
                "ns": {
                    "metrics": [
                        { "name": "dup", "type": "count", "tags": [], "description": "first" },
                        { "name": "dup", "type": "gauge", "tags": [], "description": "second" }
                    ]
                }
            }
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("duplicate metric 'ns::dup'"));
    }

    #[test]
    fn test_same_name_different_namespaces_is_ok() {
        let json = r#"{
            "tags": {},
            "namespaces": {
                "http": {
                    "metrics": [
                        { "name": "request_count", "type": "count", "tags": [], "description": "HTTP" }
                    ]
                },
                "grpc": {
                    "metrics": [
                        { "name": "request_count", "type": "count", "tags": [], "description": "gRPC" }
                    ]
                }
            }
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        schema.validate().unwrap(); // should be OK — different namespaces
    }

    #[test]
    fn test_validate_enum_tag_no_values() {
        let json = r#"{
            "tags": {
                "bad_tag": { "value_type": "enum", "export_name": "bad" }
            }
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("must have a 'values' field"));
    }

    #[test]
    fn test_validate_enum_tag_empty_values() {
        let json = r#"{
            "tags": {
                "empty": { "value_type": "enum", "values": [], "export_name": "empty" }
            }
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("at least one value"));
    }
}
