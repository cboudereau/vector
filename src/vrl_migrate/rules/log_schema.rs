use regex::Regex;
use vector_lib::config::LogSchema;

use super::RuleId;
use super::structural::replace_field;

struct FieldMapping {
    rule_id: RuleId,
    custom_name: String,
    canonical_path: &'static str,
    annotation: String,
}

const DEFAULT_NAMES: &[&str] = &["body", "time_unix_nano", "host", "source_type", "metadata"];
const STRUCTURAL_DEFAULTS: &[&str] = &["message", "timestamp", "host", "source_type"];

pub struct LogSchemaRules {
    mappings: Vec<FieldMapping>,
}

impl LogSchemaRules {
    pub fn from_schema(schema: &LogSchema) -> Self {
        let mut mappings = Vec::new();

        for (field_name, custom_value, canonical_path) in schema.non_default_fields() {
            if custom_value.is_empty() {
                continue;
            }
            // Skip if custom value is already the canonical name
            if DEFAULT_NAMES.contains(&custom_value.as_str()) {
                continue;
            }
            // Skip if structural rules (LOG-01..04) already handle this name
            if STRUCTURAL_DEFAULTS.contains(&custom_value.as_str()) {
                continue;
            }

            let rule_id = match field_name {
                "message_key" => RuleId::Ls01,
                "timestamp_key" => RuleId::Ls02,
                "host_key" => RuleId::Ls03,
                "source_type_key" => RuleId::Ls04,
                "metadata_key" => RuleId::Ls05,
                _ => continue,
            };

            let annotation = if canonical_path.is_empty() {
                format!(".{custom_value} → REVIEW (metadata_key has no standard OTLP mapping) [{rule_id}]")
            } else {
                format!(".{custom_value} → {canonical_path} [{rule_id}]")
            };

            mappings.push(FieldMapping {
                rule_id,
                custom_name: custom_value,
                canonical_path,
                annotation,
            });
        }

        Self { mappings }
    }

    pub fn apply_to_output(&self, output: &mut super::super::MigrationOutput) {
        if self.mappings.is_empty() {
            return;
        }

        let source = std::mem::take(&mut output.text);
        let lines: Vec<&str> = source.lines().collect();
        let mut result_lines: Vec<String> = Vec::with_capacity(lines.len());

        for (line_idx, line) in lines.iter().enumerate() {
            let mut current = line.to_string();
            let mut annotations: Vec<String> = Vec::new();

            for mapping in &self.mappings {
                let pattern = format!(r"\.{}\b", regex::escape(&mapping.custom_name));
                let re = Regex::new(&pattern).unwrap();

                if mapping.canonical_path.is_empty() {
                    // metadata_key: no OTLP mapping, flag for review
                    for m in re.find_iter(&current) {
                        if !super::in_string_or_comment(&current, m.start()) {
                            output.reviews.push(super::super::ReviewItem {
                                rule_id: mapping.rule_id,
                                line: line_idx + 1,
                                reason: mapping.annotation.clone(),
                            });
                            annotations.push(format!("# REVIEW: {}", mapping.annotation));
                            break;
                        }
                    }
                } else if let Some(new_text) = replace_field(&current, &re, mapping.canonical_path) {
                    output.applied.push(super::super::AppliedRule {
                        rule_id: mapping.rule_id,
                        line: line_idx + 1,
                        original: current.clone(),
                        rewritten: new_text.clone(),
                    });
                    current = new_text;
                    annotations.push(format!("# MIGRATED: {}", mapping.annotation));
                }
            }

            if annotations.is_empty() {
                result_lines.push(current);
            } else {
                let indent = super::super::leading_whitespace(&current);
                for ann in &annotations {
                    result_lines.push(format!("{indent}{ann}"));
                }
                result_lines.push(current);
            }
        }

        output.text = result_lines.join("\n");
        if output.text.is_empty() && !lines.is_empty() {
            output.text.push('\n');
        }
    }
}
