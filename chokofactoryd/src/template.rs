//! Cross-stage variable substitution (P2-3, design §5.1).
//!
//! A `shell`/`poll`/`agent_turn` stage can capture what it produced into
//! `workflow_state.payload` under `payload.stages.<stage name>` (see
//! `engine::merge_stage_capture`). This module is the other half: resolving
//! `{{ stages.<name>.<field> }}` in a later stage's `command:` or
//! `prompt_file` against that payload.
//!
//! This is deliberately *only* variable substitution. There are no
//! conditionals, no expressions, and no function calls — branching stays in
//! `on:` maps and `outcomes:` matching (§7 non-goal). That constraint is why
//! this is a hand-written scanner rather than a templating crate: the whole
//! grammar is "a dotted path into one JSON blob", and owning the parser is
//! what lets the loader reuse it to reject a bad reference at load time
//! (`WorkflowDefinition::validate`) instead of only when a task reaches the
//! stage.
//!
//! There is no escape syntax for a literal `{{`. Adding one before anything
//! needs it would be guessing; a workflow that genuinely has to emit `{{`
//! can do it from a `script_file`, whose contents are never templated.

use serde_json::Value;
use std::fmt;

const OPEN: &str = "{{";
const CLOSE: &str = "}}";

/// The only recognised root. Reserving it — rather than resolving against
/// the payload root — is what lets other engine-owned namespaces join
/// `payload` later without colliding with a workflow whose stage happens to
/// be named after one (same reasoning as `merge_stage_capture`'s).
const NAMESPACE: &str = "stages";

/// One `{{ … }}` occurrence, parsed but not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    /// The full `{{ … }}` text, kept verbatim so an error can quote back
    /// exactly what the author wrote.
    pub placeholder: String,
    /// The stage whose capture this reads.
    pub stage: String,
    /// Field path *within* that stage's captured value. Empty for a bare
    /// `{{ stages.<name> }}`, which is how a `capture: text` stage — whose
    /// payload entry is a plain string, not an object — is referenced.
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    Malformed {
        placeholder: String,
        reason: String,
    },
    UnknownNamespace {
        placeholder: String,
        root: String,
    },
    /// No capture is stored for that stage — it hasn't run yet, or it
    /// declares no `capture:` at all.
    UnresolvedStage {
        placeholder: String,
        stage: String,
    },
    UnresolvedField {
        placeholder: String,
        stage: String,
        field: String,
    },
    /// Resolved to something that can't be substituted into a command or a
    /// prompt as-is. Capture is for short structured signals — a verdict, an
    /// id, a url — not for splicing a blob into a shell command (§5.1).
    NotScalar {
        placeholder: String,
        kind: &'static str,
    },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::Malformed {
                placeholder,
                reason,
            } => {
                write!(f, "{placeholder} is not a valid reference: {reason}")
            }
            TemplateError::UnknownNamespace { placeholder, root } => write!(
                f,
                "{placeholder} reads from '{root}', but '{NAMESPACE}' is the only namespace \
                 a template can read from"
            ),
            TemplateError::UnresolvedStage { placeholder, stage } => write!(
                f,
                "{placeholder} has nothing to resolve against: stage '{stage}' has captured \
                 nothing (it has not run yet, or it declares no 'capture:')"
            ),
            TemplateError::UnresolvedField {
                placeholder,
                stage,
                field,
            } => write!(
                f,
                "{placeholder} has no value: stage '{stage}' captured no '{field}'"
            ),
            TemplateError::NotScalar { placeholder, kind } => write!(
                f,
                "{placeholder} resolves to {kind}, which cannot be substituted into a command \
                 or prompt; reference a single scalar field instead"
            ),
        }
    }
}

impl std::error::Error for TemplateError {}

enum Segment {
    Literal(String),
    Reference(TemplateRef),
}

/// Every reference in `input`, in order, for load-time validation.
///
/// Returns the same parse errors `render` would, so a definition that would
/// fail to render is rejected by the loader rather than at run time.
pub fn references(input: &str) -> Result<Vec<TemplateRef>, TemplateError> {
    Ok(parse(input)?
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Reference(reference) => Some(reference),
            Segment::Literal(_) => None,
        })
        .collect())
}

/// Substitutes every reference in `input` against a task's
/// `workflow_state.payload`.
///
/// An unresolvable reference is an error, never an empty string: silently
/// substituting nothing would hand a malformed command to `sh -c` or a
/// prompt with a hole in it to an agent, and the failure would surface
/// somewhere far away from its cause.
pub fn render(input: &str, payload: &Value) -> Result<String, TemplateError> {
    // Much the commonest case — most commands and prompts have no
    // placeholders at all — and it keeps the parse off the hot path.
    if !input.contains(OPEN) {
        return Ok(input.to_string());
    }

    let mut rendered = String::with_capacity(input.len());
    for segment in parse(input)? {
        match segment {
            Segment::Literal(text) => rendered.push_str(&text),
            Segment::Reference(reference) => rendered.push_str(&resolve(&reference, payload)?),
        }
    }
    Ok(rendered)
}

fn parse(input: &str) -> Result<Vec<Segment>, TemplateError> {
    let mut segments = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find(OPEN) {
        if start > 0 {
            segments.push(Segment::Literal(rest[..start].to_string()));
        }
        let after_open = &rest[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            return Err(TemplateError::Malformed {
                placeholder: rest[start..].to_string(),
                reason: format!("no closing `{CLOSE}`"),
            });
        };
        let body = &after_open[..end];
        segments.push(Segment::Reference(parse_reference(body)?));
        rest = &after_open[end + CLOSE.len()..];
    }

    if !rest.is_empty() {
        segments.push(Segment::Literal(rest.to_string()));
    }
    Ok(segments)
}

fn parse_reference(body: &str) -> Result<TemplateRef, TemplateError> {
    let placeholder = format!("{OPEN}{body}{CLOSE}");
    let path = body.trim();

    if path.is_empty() {
        return Err(TemplateError::Malformed {
            placeholder,
            reason: "it is empty".to_string(),
        });
    }
    // Whitespace is tolerated *around* the path but not inside it, so that a
    // typo like `{{ stages.open pr.url }}` is reported here rather than
    // becoming a stage name that can never match.
    if path.chars().any(char::is_whitespace) {
        return Err(TemplateError::Malformed {
            placeholder,
            reason: "it has whitespace inside the path".to_string(),
        });
    }

    let mut parts = path.split('.');
    let root = parts.next().expect("split always yields at least one part");
    if root != NAMESPACE {
        return Err(TemplateError::UnknownNamespace {
            placeholder,
            root: root.to_string(),
        });
    }

    let remainder: Vec<&str> = parts.collect();
    if remainder.iter().any(|part| part.is_empty()) {
        return Err(TemplateError::Malformed {
            placeholder,
            reason: "it has an empty path segment".to_string(),
        });
    }
    let Some((stage, fields)) = remainder.split_first() else {
        return Err(TemplateError::Malformed {
            placeholder,
            reason: format!("it names no stage (expected `{NAMESPACE}.<stage>`)"),
        });
    };

    Ok(TemplateRef {
        placeholder,
        stage: (*stage).to_string(),
        path: fields.iter().map(|field| (*field).to_string()).collect(),
    })
}

fn resolve(reference: &TemplateRef, payload: &Value) -> Result<String, TemplateError> {
    let mut value = payload
        .get(NAMESPACE)
        .and_then(|stages| stages.get(&reference.stage))
        .ok_or_else(|| TemplateError::UnresolvedStage {
            placeholder: reference.placeholder.clone(),
            stage: reference.stage.clone(),
        })?;

    for (depth, field) in reference.path.iter().enumerate() {
        // `Value::get` on a non-object is `None`, so this also covers
        // indexing into a `capture: text` stage's plain string.
        value = value
            .get(field)
            .ok_or_else(|| TemplateError::UnresolvedField {
                placeholder: reference.placeholder.clone(),
                stage: reference.stage.clone(),
                field: reference.path[..=depth].join("."),
            })?;
    }

    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Null => Err(TemplateError::NotScalar {
            placeholder: reference.placeholder.clone(),
            kind: "null",
        }),
        Value::Object(_) => Err(TemplateError::NotScalar {
            placeholder: reference.placeholder.clone(),
            kind: "an object",
        }),
        Value::Array(_) => Err(TemplateError::NotScalar {
            placeholder: reference.placeholder.clone(),
            kind: "an array",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({
            "stages": {
                "open_pr": { "number": 42, "url": "https://example.test/pr/42", "draft": true },
                "review": { "outcome": "approved", "detail": { "note": "ship it" } },
                "checks": "SUCCESS",
                "empty": { "value": null }
            }
        })
    }

    #[test]
    fn renders_a_single_field() {
        let rendered = render("gh pr checks {{ stages.open_pr.number }}", &payload()).unwrap();
        assert_eq!(rendered, "gh pr checks 42");
    }

    #[test]
    fn renders_a_nested_path() {
        let rendered = render("{{ stages.review.detail.note }}", &payload()).unwrap();
        assert_eq!(rendered, "ship it");
    }

    #[test]
    fn renders_a_bare_stage_reference_for_a_text_capture() {
        let rendered = render("state={{ stages.checks }}", &payload()).unwrap();
        assert_eq!(rendered, "state=SUCCESS");
    }

    #[test]
    fn tolerates_whitespace_around_the_path() {
        let payload = payload();
        assert_eq!(
            render("{{stages.open_pr.number}}", &payload).unwrap(),
            render("{{      stages.open_pr.number   }}", &payload).unwrap()
        );
    }

    #[test]
    fn renders_several_placeholders_in_one_string() {
        let rendered = render(
            "pr {{ stages.open_pr.number }} at {{ stages.open_pr.url }} is {{ stages.review.outcome }}",
            &payload(),
        )
        .unwrap();
        assert_eq!(rendered, "pr 42 at https://example.test/pr/42 is approved");
    }

    #[test]
    fn renders_booleans_and_numbers_as_plain_scalars() {
        assert_eq!(
            render("{{ stages.open_pr.draft }}", &payload()).unwrap(),
            "true"
        );
        assert_eq!(
            render("{{ stages.open_pr.number }}", &payload()).unwrap(),
            "42"
        );
    }

    #[test]
    fn leaves_text_without_placeholders_untouched() {
        let input = "gh pr create --fill { not a placeholder }";
        assert_eq!(render(input, &json!({})).unwrap(), input);
    }

    #[test]
    fn an_unterminated_placeholder_is_malformed() {
        let err = render("echo {{ stages.open_pr.number", &payload()).unwrap_err();
        assert!(matches!(err, TemplateError::Malformed { .. }), "{err}");
    }

    #[test]
    fn whitespace_inside_the_path_is_malformed() {
        let err = render("{{ stages.open pr.number }}", &payload()).unwrap_err();
        assert!(matches!(err, TemplateError::Malformed { .. }), "{err}");
    }

    #[test]
    fn an_empty_path_segment_is_malformed() {
        let err = render("{{ stages..number }}", &payload()).unwrap_err();
        assert!(matches!(err, TemplateError::Malformed { .. }), "{err}");
    }

    #[test]
    fn a_reference_naming_no_stage_is_malformed() {
        let err = render("{{ stages }}", &payload()).unwrap_err();
        assert!(matches!(err, TemplateError::Malformed { .. }), "{err}");
    }

    #[test]
    fn a_non_stages_root_is_rejected() {
        let err = render("{{ task.id }}", &payload()).unwrap_err();
        assert!(
            matches!(&err, TemplateError::UnknownNamespace { root, .. } if root == "task"),
            "{err}"
        );
    }

    #[test]
    fn a_stage_that_has_captured_nothing_is_unresolved() {
        let err = render("{{ stages.never_ran.url }}", &payload()).unwrap_err();
        assert!(
            matches!(&err, TemplateError::UnresolvedStage { stage, .. } if stage == "never_ran"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_field_is_unresolved_and_names_the_full_path() {
        let err = render("{{ stages.review.detail.missing }}", &payload()).unwrap_err();
        assert!(
            matches!(&err, TemplateError::UnresolvedField { field, .. } if field == "detail.missing"),
            "{err}"
        );
    }

    #[test]
    fn indexing_into_a_text_capture_is_unresolved_rather_than_a_panic() {
        let err = render("{{ stages.checks.state }}", &payload()).unwrap_err();
        assert!(
            matches!(err, TemplateError::UnresolvedField { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_object_cannot_be_substituted() {
        let err = render("{{ stages.open_pr }}", &payload()).unwrap_err();
        assert!(
            matches!(&err, TemplateError::NotScalar { kind, .. } if *kind == "an object"),
            "{err}"
        );
    }

    #[test]
    fn a_null_capture_cannot_be_substituted() {
        let err = render("{{ stages.empty.value }}", &payload()).unwrap_err();
        assert!(
            matches!(&err, TemplateError::NotScalar { kind, .. } if *kind == "null"),
            "{err}"
        );
    }

    #[test]
    fn references_lists_every_placeholder_in_order() {
        let found = references("a {{ stages.one.x }} b {{ stages.two }} c").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].stage, "one");
        assert_eq!(found[0].path, vec!["x".to_string()]);
        assert_eq!(found[1].stage, "two");
        assert!(found[1].path.is_empty());
    }

    #[test]
    fn references_surfaces_the_same_parse_errors_render_would() {
        assert!(references("{{ stages. }}").is_err());
        assert!(references("{{ nope.x }}").is_err());
        assert!(references("{{ unterminated").is_err());
    }

    #[test]
    fn references_is_empty_for_plain_text() {
        assert!(references("gh pr create --fill").unwrap().is_empty());
    }
}
