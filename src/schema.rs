use serde::Deserialize;
use std::collections::HashMap;

/// Top-level metrics schema parsed from the JSON definition file.
#[derive(Debug, Deserialize)]
pub struct MetricsSchema {
    pub tags: HashMap<String, TagDefinition>,
    pub metrics: Vec<MetricDefinition>,
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

impl MetricsSchema {
    /// Parse a metrics schema from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
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

        // Check for duplicate metric names
        let mut seen_names = HashMap::new();
        for (i, metric) in self.metrics.iter().enumerate() {
            if let Some(prev_idx) = seen_names.insert(&metric.name, i) {
                errors.push(format!(
                    "duplicate metric name '{}' (indices {prev_idx} and {i})",
                    metric.name
                ));
            }
        }

        // Check metric tag references point to defined tags
        for metric in &self.metrics {
            for tag_ref in &metric.tags {
                if !self.tags.contains_key(&tag_ref.tag) {
                    errors.push(format!(
                        "metric '{}': references undefined tag '{}'",
                        metric.name, tag_ref.tag
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_schema() {
        let json = r#"{"tags": {}, "metrics": []}"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        assert!(schema.tags.is_empty());
        assert!(schema.metrics.is_empty());
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
                },
                {
                    "name": "resolver_latency",
                    "type": "histogram",
                    "tags": [
                        { "tag": "endpoint" },
                        { "tag": "status", "export_name": "resolver_status" },
                        { "tag": "endpoint", "export_name": "resolver_fqn", "optional": true }
                    ],
                    "description": "Resolver execution latency"
                }
            ]
        }"#;

        let schema = MetricsSchema::from_json(json).unwrap();
        assert_eq!(schema.tags.len(), 2);
        assert_eq!(schema.metrics.len(), 2);

        // Check tag definitions
        let status = &schema.tags["status"];
        assert_eq!(status.value_type, TagValueType::Enum);
        assert_eq!(
            status.values.as_ref().unwrap(),
            &["success", "failure", "timeout"]
        );
        assert_eq!(status.export_name, "status");

        let endpoint = &schema.tags["endpoint"];
        assert_eq!(endpoint.value_type, TagValueType::String);
        assert!(endpoint.values.is_none());

        // Check metric definitions
        let req_lat = &schema.metrics[0];
        assert_eq!(req_lat.name, "request_latency");
        assert_eq!(req_lat.metric_type, MetricType::Histogram);
        assert_eq!(req_lat.tags.len(), 2);
        assert_eq!(req_lat.description, "HTTP request latency");

        // Check tag references with overrides
        let res_lat = &schema.metrics[1];
        assert_eq!(res_lat.tags[1].tag, "status");
        assert_eq!(
            res_lat.tags[1].export_name.as_deref(),
            Some("resolver_status")
        );
        assert!(!res_lat.tags[1].optional);
        assert!(res_lat.tags[2].optional);

        schema.validate().unwrap();
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
    fn test_validate_missing_tag_ref() {
        let json = r#"{
            "tags": {},
            "metrics": [
                {
                    "name": "m1",
                    "type": "count",
                    "tags": [{ "tag": "nonexistent" }],
                    "description": "test"
                }
            ]
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("undefined tag 'nonexistent'"));
    }

    #[test]
    fn test_validate_duplicate_metric_names() {
        let json = r#"{
            "tags": {},
            "metrics": [
                { "name": "dup", "type": "count", "tags": [], "description": "first" },
                { "name": "dup", "type": "gauge", "tags": [], "description": "second" }
            ]
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("duplicate metric name 'dup'"));
    }

    #[test]
    fn test_validate_enum_tag_no_values() {
        let json = r#"{
            "tags": {
                "bad_tag": { "value_type": "enum", "export_name": "bad" }
            },
            "metrics": []
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
            },
            "metrics": []
        }"#;
        let schema = MetricsSchema::from_json(json).unwrap();
        let err = schema.validate().unwrap_err();
        assert!(err.errors[0].contains("at least one value"));
    }
}
