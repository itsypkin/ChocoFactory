//! Level suffixes on model strings (#336): `model:<level>`.
//!
//! The only place that knows the level lists, the default level and how a
//! model string splits. Claude roles take an effort level (`--effort`); omp
//! roles take a thinking level (`--thinking`).

/// The levels `claude --help` lists for `--effort`.
pub const CLAUDE_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The values omp's `--thinking` accepts.
pub const OMP_THINKING_LEVELS: [&str; 9] = [
    "off", "minimal", "low", "medium", "high", "xhigh", "max", "auto", "inherit",
];

/// The level every role runs at when its model string names none.
pub const DEFAULT_LEVEL: &str = "high";

/// Splits `model` into the id and a candidate level suffix. The text after
/// the last `:` counts as a suffix only when it is empty or all ASCII
/// letters, so ids that contain colons (Bedrock `...-v1:0`, ARNs) stay whole.
fn split(model: &str) -> (&str, Option<&str>) {
    match model.rsplit_once(':') {
        Some((id, suffix)) if suffix.chars().all(|c| c.is_ascii_alphabetic()) => (id, Some(suffix)),
        _ => (model, None),
    }
}

/// Claude's rule: the bare model id and the effort level to pass. No suffix
/// gives [`DEFAULT_LEVEL`]; a suffix that is not a claude level is an error
/// naming the role, the model and the suffix.
pub fn claude_model_and_level<'a>(
    role: &str,
    model: &'a str,
) -> Result<(&'a str, &'a str), String> {
    let (id, suffix) = split(model);
    let Some(suffix) = suffix else {
        return Ok((model, DEFAULT_LEVEL));
    };
    if let Some(level) = CLAUDE_EFFORT_LEVELS.iter().find(|l| **l == suffix) {
        return Ok((id, level));
    }
    let why = if suffix.is_empty() {
        "the level after the `:` is empty".to_string()
    } else if OMP_THINKING_LEVELS.contains(&suffix) {
        format!("`:{suffix}` is an omp-only level")
    } else {
        format!("`:{suffix}` is not an effort level")
    };
    Err(format!(
        "role `{role}`: model `{model}` is not valid for claude: {why}. \
         Claude effort levels are: {}",
        CLAUDE_EFFORT_LEVELS.join(", ")
    ))
}

/// omp's rule: does the model string end in one of omp's own levels? Any
/// other suffix (`p/m:latest`) is part of the model id.
pub fn omp_has_level(model: &str) -> bool {
    matches!(split(model), (_, Some(suffix)) if OMP_THINKING_LEVELS.contains(&suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_splits_and_defaults() {
        assert_eq!(
            claude_model_and_level("r", "claude-sonnet-5-5"),
            Ok(("claude-sonnet-5-5", "high"))
        );
        for level in CLAUDE_EFFORT_LEVELS {
            assert_eq!(
                claude_model_and_level("r", &format!("claude-sonnet-5-5:{level}")),
                Ok(("claude-sonnet-5-5", level))
            );
        }
        assert_eq!(
            claude_model_and_level("r", "sonnet[1m]:low"),
            Ok(("sonnet[1m]", "low"))
        );
    }

    #[test]
    fn claude_keeps_ids_with_colons_whole() {
        for id in [
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123",
        ] {
            assert_eq!(claude_model_and_level("r", id), Ok((id, "high")));
            assert!(!omp_has_level(id));
        }
    }

    #[test]
    fn claude_refuses_other_suffixes_naming_role_model_and_suffix() {
        for (model, needle) in [
            ("claude-opus-5-5:minimal", "omp-only"),
            ("claude-opus-5-5:auto", "omp-only"),
            ("claude-opus-5-5:hihg", "not an effort level"),
            ("claude-opus-5-5:HIGH", "not an effort level"),
            ("claude-opus-5-5:", "empty"),
        ] {
            let err = claude_model_and_level("coder", model).unwrap_err();
            assert!(err.contains("`coder`"), "{err}");
            assert!(err.contains(model), "{err}");
            assert!(err.contains(needle), "{err}");
            assert!(err.contains("low, medium, high, xhigh, max"), "{err}");
        }
    }

    #[test]
    fn omp_level_only_for_its_own_levels() {
        for level in OMP_THINKING_LEVELS {
            assert!(omp_has_level(&format!("p/m:{level}")), "{level}");
        }
        assert!(!omp_has_level("p/m"));
        assert!(!omp_has_level("p/m:latest"));
        assert!(!omp_has_level("x:"));
        assert!(!omp_has_level("x:HIGH"));
    }
}
