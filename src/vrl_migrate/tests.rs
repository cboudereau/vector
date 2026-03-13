use super::*;

fn assert_migrates(input: &str, expected: &str) {
    let output = migrate(input);
    assert_eq!(
        output.text, expected,
        "\n--- input ---\n{input}\n--- expected ---\n{expected}\n--- got ---\n{}",
        output.text
    );
}

fn assert_unchanged(input: &str) {
    let output = migrate(input);
    assert_eq!(
        output.text, input,
        "\n--- input ---\n{input}\n--- should be unchanged but got ---\n{}",
        output.text
    );
}

fn assert_has_review(input: &str, rule: RuleId) {
    let output = migrate(input);
    assert!(
        output.reviews.iter().any(|r| r.rule_id == rule),
        "Expected REVIEW for {rule} in:\n{input}\nGot reviews: {:?}",
        output.reviews
    );
}

// ═══════════════════════════════════════════════════════════════
// Pass 1: Structural rewrites
// ═══════════════════════════════════════════════════════════════

#[test]
fn log01_message_standalone() {
    assert_migrates(
        ".foo = .message",
        "# MIGRATED: .message → . [LOG-01]\n.foo = .",
    );
}

#[test]
fn log01_message_in_function() {
    assert_migrates(
        "to_string(.message)",
        "# MIGRATED: .message → . [LOG-01]\nto_string(.)",
    );
}

#[test]
fn log01_message_not_in_string() {
    assert_unchanged(r#".foo = "has .message in string""#);
}

#[test]
fn log01_message_not_prefix() {
    assert_unchanged(".message_type = 1");
}

#[test]
fn log02_timestamp() {
    assert_migrates(
        ".ts = .timestamp",
        "# MIGRATED: .timestamp → .time_unix_nano [LOG-02]\n.ts = .time_unix_nano",
    );
}

#[test]
fn log02_timestamp_not_prefix() {
    assert_unchanged(".timestamp_format = \"rfc3339\"");
}

#[test]
fn log03_source_type() {
    assert_migrates(
        "x = .source_type",
        "# MIGRATED: .source_type → .attributes.\"pipeline.source_type\" [LOG-03]\nx = .attributes.\"pipeline.source_type\"",
    );
}

#[test]
fn log04_host() {
    assert_migrates(
        ".h = .host",
        "# MIGRATED: .host → .resource.attributes.\"host.name\" [LOG-04]\n.h = .resource.attributes.\"host.name\"",
    );
}

#[test]
fn log04_host_not_prefix() {
    assert_unchanged(".hostname = \"foo\"");
}

#[test]
fn log05_tags_standalone() {
    assert_migrates(
        "t = .tags",
        "# MIGRATED: .tags → .attributes [LOG-05]\nt = .attributes",
    );
}

#[test]
fn log06_tags_key() {
    assert_migrates(
        "env = .tags.environment",
        "# MIGRATED: .tags.<key> → .attributes.\"<key>\" [LOG-06]\nenv = .attributes.\"environment\"",
    );
}

#[test]
fn log07_level() {
    assert_migrates(
        ".severity_text = .level",
        "# MIGRATED: .level/.severity → .severity_text [LOG-07]\n.severity_text = .severity_text",
    );
}

#[test]
fn log07_severity() {
    assert_migrates(
        "s = .severity",
        "# MIGRATED: .level/.severity → .severity_text [LOG-07]\ns = .severity_text",
    );
}

#[test]
fn meta01_source_type() {
    assert_migrates(
        "src = %vector.source_type",
        "# MIGRATED: %vector.source_type → %pipeline.source_type [META-01]\nsrc = %pipeline.source_type",
    );
}

#[test]
fn meta02_source_id() {
    assert_migrates(
        "id = %vector.source_id",
        "# MIGRATED: %vector.source_id → %pipeline.source_id [META-02]\nid = %pipeline.source_id",
    );
}

// ═══════════════════════════════════════════════════════════════
// Pass 2: Semantic rewrites
// ═══════════════════════════════════════════════════════════════

#[test]
fn sem02_exists_message() {
    assert_migrates(
        "if exists(.message) {",
        "# MIGRATED: .message → . [LOG-01]\n# MIGRATED: exists(.) → true (root always exists) [SEM-02]\nif true {",
    );
}

#[test]
fn sem03_del_message() {
    let output = migrate("del(.message)");
    assert!(output.text.contains("MIGRATED: .message → . [LOG-01]"));
    assert!(output.text.contains("REVIEW: del(.) would delete the entire event"));
}

#[test]
fn sem05_parse_json_message() {
    let output = migrate("parsed = parse_json(.message)");
    assert!(output.text.contains("parse_json(string!(.))"),
        "Expected parse_json(string!(.)) in:\n{}", output.text);
}

#[test]
fn sem06_is_string_message() {
    let output = migrate("if is_string(.message) {");
    assert!(output.text.contains("is_string(.)"),
        "Expected is_string(.) in:\n{}", output.text);
}

#[test]
fn sem07_assert_eq_message() {
    let output = migrate(r#"assert_eq!(.message, "hello")"#);
    assert!(output.text.contains("assert_eq!(string!(.),"),
        "Expected assert_eq!(string!(.), ...) in:\n{}", output.text);
}

// ═══════════════════════════════════════════════════════════════
// Pass 3: Metric rewrites
// ═══════════════════════════════════════════════════════════════

#[test]
fn met02_namespace() {
    assert_migrates(
        "ns = .namespace",
        "# MIGRATED: .namespace → .attributes.\"metric.namespace\" [MET-02]\nns = .attributes.\"metric.namespace\"",
    );
}

#[test]
fn met05_kind_review() {
    assert_has_review(".kind == \"incremental\"", RuleId::Met05);
}

#[test]
fn met06_counter_value() {
    assert_migrates(
        "v = .value.counter.value",
        "# MIGRATED: .value.counter.value → .data_points[0].as_double [MET-06]\nv = .data_points[0].as_double",
    );
}

#[test]
fn met07_gauge_value() {
    assert_migrates(
        "v = .value.gauge.value",
        "# MIGRATED: .value.gauge.value → .data_points[0].as_double [MET-07]\nv = .data_points[0].as_double",
    );
}

// ═══════════════════════════════════════════════════════════════
// Multi-line programs
// ═══════════════════════════════════════════════════════════════

#[test]
fn multi_line_program() {
    let input = r#".parsed = parse_json(.message)
.level = "info"
.host = "myhost"
.tags.env = "prod""#;

    let output = migrate(input);
    assert!(output.text.contains("parse_json(string!(.))"));
    assert!(output.text.contains(".severity_text"));
    assert!(output.text.contains(r#".resource.attributes."host.name""#));
    assert!(output.text.contains(r#".attributes."env""#));
}

#[test]
fn preserves_comments() {
    assert_unchanged("# this is a comment with .message in it");
}

#[test]
fn preserves_strings() {
    assert_unchanged(r#".foo = "the .message field""#);
}

// ═══════════════════════════════════════════════════════════════
// Diff output
// ═══════════════════════════════════════════════════════════════

#[test]
fn diff_shows_changes() {
    let input = ".foo = .message";
    let d = diff(input, None);
    assert!(d.contains("-.foo = .message"));
    assert!(d.contains("+.foo = ."));
}

#[test]
fn diff_empty_when_unchanged() {
    let input = ".foo = .bar";
    let d = diff(input, None);
    assert!(d.is_empty());
}
