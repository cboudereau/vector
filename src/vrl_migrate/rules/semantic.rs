use std::sync::LazyLock;

use regex::Regex;

use super::{replace_outside_strings, RewriteResult, Rule, RuleId};

/// Semantic rules run AFTER structural Pass 1.
/// At this point, `.message` has already been rewritten to `.` by LOG-01.
/// These rules tighten `.` usage in non-root contexts.
pub static RULES: [Rule; 7] = [
    Rule { id: RuleId::Sem03, apply: apply_sem03 },
    Rule { id: RuleId::Sem02, apply: apply_sem02 },
    Rule { id: RuleId::Sem04, apply: apply_sem04 },
    Rule { id: RuleId::Sem05, apply: apply_sem05 },
    Rule { id: RuleId::Sem06, apply: apply_sem06 },
    Rule { id: RuleId::Sem07, apply: apply_sem07 },
    Rule { id: RuleId::Sem01, apply: apply_sem01 },
];

// SEM-01: get_field!(., "message") → string!(.)
// After LOG-01, the pattern is get_field!(., ".") or similar.
// We match the original pattern since it may not have been in the LOG-01 rewrite scope.
static RE_GET_FIELD_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"get_field!\(\s*\.\s*,\s*"message"\s*\)"#).unwrap()
});

fn apply_sem01(line: &str) -> RewriteResult {
    match replace_outside_strings(line, &RE_GET_FIELD_MESSAGE, "string!(.)") {
        Some(new) => RewriteResult::Rewritten(
            new,
            r#"get_field!(., "message") → string!(.) [SEM-01]"#.into(),
        ),
        None => RewriteResult::NoMatch,
    }
}

// SEM-02: exists(.message) → true (after LOG-01: exists(.) → true)
// Root always exists.
static RE_EXISTS_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"exists\(\s*\.\s*\)").unwrap()
});
static RE_EXISTS_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"exists\(\s*\.message\s*\)").unwrap()
});

fn apply_sem02(line: &str) -> RewriteResult {
    if let Some(new) = replace_outside_strings(line, &RE_EXISTS_ROOT, "true") {
        return RewriteResult::Rewritten(new, "exists(.) → true (root always exists) [SEM-02]".into());
    }
    if let Some(new) = replace_outside_strings(line, &RE_EXISTS_MESSAGE, "true") {
        return RewriteResult::Rewritten(new, "exists(.message) → true (root always exists) [SEM-02]".into());
    }
    RewriteResult::NoMatch
}

// SEM-03: del(.message) → REVIEW (after LOG-01: del(.) → REVIEW)
static RE_DEL_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"del\(\s*\.\s*\)").unwrap()
});
static RE_DEL_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"del\(\s*\.message\s*\)").unwrap()
});

fn apply_sem03(line: &str) -> RewriteResult {
    if RE_DEL_ROOT.is_match(line) && !super::in_string_or_comment(line, RE_DEL_ROOT.find(line).unwrap().start()) {
        return RewriteResult::NeedsReview(
            "del(.) would delete the entire event — did you mean to clear the body? [SEM-03]".into(),
        );
    }
    if RE_DEL_MESSAGE.is_match(line) && !super::in_string_or_comment(line, RE_DEL_MESSAGE.find(line).unwrap().start()) {
        return RewriteResult::NeedsReview(
            "del(.message) → del(.) would delete the entire event [SEM-03]".into(),
        );
    }
    RewriteResult::NoMatch
}

// SEM-04: encode_json(.message) → encode_json(.)
// After LOG-01 this is already encode_json(.), but if the original wasn't caught, handle it.
static RE_ENCODE_JSON_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"encode_json\(\s*\.message\s*\)").unwrap()
});

fn apply_sem04(line: &str) -> RewriteResult {
    match replace_outside_strings(line, &RE_ENCODE_JSON_MESSAGE, "encode_json(.)") {
        Some(new) => RewriteResult::Rewritten(
            new,
            "encode_json(.message) → encode_json(.) [SEM-04]".into(),
        ),
        None => RewriteResult::NoMatch,
    }
}

// SEM-05: parse_json(.message) → parse_json(string!(.))
// After LOG-01: parse_json(.) → parse_json(string!(.))
static RE_PARSE_JSON_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"parse_json\(\s*\.\s*\)").unwrap()
});
static RE_PARSE_JSON_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"parse_json\(\s*\.message\s*\)").unwrap()
});

fn apply_sem05(line: &str) -> RewriteResult {
    if let Some(new) = replace_outside_strings(line, &RE_PARSE_JSON_ROOT, "parse_json(string!(.))") {
        return RewriteResult::Rewritten(new, "parse_json(.) → parse_json(string!(.)) [SEM-05]".into());
    }
    if let Some(new) = replace_outside_strings(line, &RE_PARSE_JSON_MESSAGE, "parse_json(string!(.))") {
        return RewriteResult::Rewritten(new, "parse_json(.message) → parse_json(string!(.)) [SEM-05]".into());
    }
    RewriteResult::NoMatch
}

// SEM-06: is_string(.message) → is_string(.)
// After LOG-01, this is already is_string(.), no further transformation needed
// since `.` is the root and type check on root is valid.
static RE_IS_STRING_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"is_string\(\s*\.message\s*\)").unwrap()
});

fn apply_sem06(line: &str) -> RewriteResult {
    match replace_outside_strings(line, &RE_IS_STRING_MESSAGE, "is_string(.)") {
        Some(new) => RewriteResult::Rewritten(
            new,
            "is_string(.message) → is_string(.) [SEM-06]".into(),
        ),
        None => RewriteResult::NoMatch,
    }
}

// SEM-07: assert_eq!(.message, ...) → assert_eq!(string!(.), ...)
// After LOG-01, the pattern is assert_eq!(., ...) — match both forms.
static RE_ASSERT_EQ_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"assert_eq!\(\s*\.message\s*,").unwrap()
});
static RE_ASSERT_EQ_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"assert_eq!\(\s*\.\s*,").unwrap()
});

fn apply_sem07(line: &str) -> RewriteResult {
    if let Some(new) = replace_outside_strings(line, &RE_ASSERT_EQ_MESSAGE, "assert_eq!(string!(.),") {
        return RewriteResult::Rewritten(
            new,
            "assert_eq!(.message, ...) → assert_eq!(string!(.), ...) [SEM-07]".into(),
        );
    }
    if let Some(new) = replace_outside_strings(line, &RE_ASSERT_EQ_ROOT, "assert_eq!(string!(.),") {
        return RewriteResult::Rewritten(
            new,
            "assert_eq!(., ...) → assert_eq!(string!(.), ...) [SEM-07]".into(),
        );
    }
    RewriteResult::NoMatch
}
