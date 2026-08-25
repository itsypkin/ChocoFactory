//! Cross-stage variable substitution (P2-3, design §5.1).
//!
//! A `shell`/`poll`/`agent_turn` stage can capture what it produced into
//! `workflow_state.payload` under `payload.stages.<stage name>` (see
//! `engine::merge_stage_capture`). This module is the other half: resolving
//! `{{ stages.<name>.<field> }}` in a later stage's `command:` or
//! `prompt_file` against that payload.
//!
//! A second root, `{{ task.<field> }}` (`input`/`title`), reads a sibling
//! key seeded once by `start_task` rather than any stage's `capture:` — it's
//! how a task's own description reaches a `prompt_file` (P2-7a). `stages`
//! and `task` are the only two roots a template can read from.
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

/// The `stages` root: a stage's own capture, reserved instead of resolving
/// against the payload root so other engine-owned namespaces (like `task`)
/// can join `payload` without colliding with a workflow whose stage happens
/// to be named after one (same reasoning as `merge_stage_capture`'s).
const NAMESPACE: &str = "stages";

/// The `task` root: the task's own title/initial input (P2-7a), seeded once
/// by `start_task` rather than any stage's `capture:`.
const TASK_NAMESPACE: &str = "task";

/// Which of the two recognised roots a reference reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// `stages.<stage>.<path>` — a stage's capture.
    Stage(String),
    /// `task.<path>` — the task's own title/initial input.
    Task,
}

/// One `{{ … }}` occurrence, parsed but not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    /// The full `{{ … }}` text, kept verbatim so an error can quote back
    /// exactly what the author wrote.
    pub placeholder: String,
    pub root: Root,
    /// Field path within the root's value. Empty for a bare `{{ stages.<name> }}`
    /// or `{{ task }}`, which is how a plain-string value — a `capture: text`
    /// stage's payload entry — is referenced.
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
    /// The `task` root has no such field — only `input`/`title` are ever
    /// set, unlike a stage's capture there is no "hasn't run yet" state for
    /// it to be missing entirely (`start_task` always seeds it), so this is
    /// the only task-side counterpart to `UnresolvedField`.
    UnresolvedTaskField {
        placeholder: String,
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

impl TemplateError {
    /// Whether this is a *missing value* (the reference parsed fine but
    /// names something that isn't there this run) rather than broken
    /// *syntax* (`Malformed`/`UnknownNamespace`, both raised by `parse`,
    /// before any payload lookup). `render` uses this to decide what
    /// substitutes as an empty string (#60) versus what still aborts the
    /// whole render — there's no sensible empty-string fallback for
    /// nonsense the author actually typed wrong, only for a value that
    /// legitimately doesn't exist yet.
    fn is_missing_value(&self) -> bool {
        matches!(
            self,
            TemplateError::UnresolvedStage { .. }
                | TemplateError::UnresolvedField { .. }
                | TemplateError::UnresolvedTaskField { .. }
                | TemplateError::NotScalar { .. }
        )
    }
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
                "{placeholder} reads from '{root}', but '{NAMESPACE}' and '{TASK_NAMESPACE}' \
                 are the only namespaces a template can read from"
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
            TemplateError::UnresolvedTaskField { placeholder, field } => {
                write!(f, "{placeholder} has no value: the task has no '{field}'")
            }
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
/// `workflow_state.payload`, returning the rendered text alongside every
/// placeholder that had to fall back to an empty string (#60) — empty when
/// nothing did.
///
/// Broken *syntax* (`Malformed`/`UnknownNamespace`) still aborts the whole
/// render: there's no sensible way to substitute nonsense, and the loader
/// already rejects it before a task ever runs (`WorkflowDefinition::
/// validate`). A reference that parses fine but names a *missing value* —
/// captures nothing yet, is a scalar the capture doesn't carry, or resolves
/// to something that isn't scalar at all — is different: the loader can
/// only check that the reference parses and names a stage that captures
/// *something*, never whether this run's captured JSON actually carries
/// the field, so treating that as fatal would kill a task for a condition
/// nothing earlier could have caught. It substitutes as `""` instead, and
/// the caller decides how to surface `unresolved` (a note on the task's
/// event timeline, today — see `engine::record_unresolved_template_note`).
pub fn render(input: &str, payload: &Value) -> Result<(String, Vec<String>), TemplateError> {
    // Much the commonest case — most commands and prompts have no
    // placeholders at all — and it keeps the parse off the hot path.
    if !input.contains(OPEN) {
        return Ok((input.to_string(), Vec::new()));
    }

    let mut rendered = String::with_capacity(input.len());
    let mut unresolved = Vec::new();
    for segment in parse(input)? {
        match segment {
            Segment::Literal(text) => rendered.push_str(&text),
            Segment::Reference(reference) => match resolve(&reference, payload) {
                Ok(text) => rendered.push_str(&text),
                Err(err) if err.is_missing_value() => unresolved.push(reference.placeholder),
                Err(err) => return Err(err),
            },
        }
    }
    Ok((rendered, unresolved))
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
    let root_name = parts.next().expect("split always yields at least one part");

    let remainder: Vec<&str> = parts.collect();
    if remainder.iter().any(|part| part.is_empty()) {
        return Err(TemplateError::Malformed {
            placeholder,
            reason: "it has an empty path segment".to_string(),
        });
    }

    match root_name {
        NAMESPACE => {
            let Some((stage, fields)) = remainder.split_first() else {
                return Err(TemplateError::Malformed {
                    placeholder,
                    reason: format!("it names no stage (expected `{NAMESPACE}.<stage>`)"),
                });
            };
            Ok(TemplateRef {
                placeholder,
                root: Root::Stage((*stage).to_string()),
                path: fields.iter().map(|field| (*field).to_string()).collect(),
            })
        }
        // Unlike `stages`, there's no stage-name segment to peel off first —
        // `task` resolves directly to a location (§ `resolve`), the same way
        // `stages.<stage>` does once its own mandatory segment is consumed.
        // A path-less `{{ task }}` is syntactically fine and simply resolves
        // to a whole object, rejected as `NotScalar` at render time exactly
        // like a path-less `{{ stages.<stage> }}` is.
        TASK_NAMESPACE => Ok(TemplateRef {
            placeholder,
            root: Root::Task,
            path: remainder.iter().map(|field| (*field).to_string()).collect(),
        }),
        other => Err(TemplateError::UnknownNamespace {
            placeholder,
            root: other.to_string(),
        }),
    }
}

// A `{{ task.… }}` reference resolves against a payload with no `task` key
// (a row from before `start_task` seeded it, or — defensively — one that
// somehow never got it) the same as an unresolved field under it: the field
// loop below reports whichever field was actually asked for, instead of a
// separate "namespace missing" error nothing else in this module has an
// analog for (a bare `{{ task }}` falls through to `NotScalar { kind: "null" }`
// instead, same as any other reference resolving to `null` does).
const MISSING_TASK: Value = Value::Null;

fn resolve(reference: &TemplateRef, payload: &Value) -> Result<String, TemplateError> {
    let mut value = match &reference.root {
        Root::Stage(stage) => payload
            .get(NAMESPACE)
            .and_then(|stages| stages.get(stage))
            .ok_or_else(|| TemplateError::UnresolvedStage {
                placeholder: reference.placeholder.clone(),
                stage: stage.clone(),
            })?,
        Root::Task => payload.get(TASK_NAMESPACE).unwrap_or(&MISSING_TASK),
    };

    for (depth, field) in reference.path.iter().enumerate() {
        // `Value::get` on a non-object is `None`, so this also covers
        // indexing into a `capture: text` stage's plain string.
        value = value.get(field).ok_or_else(|| match &reference.root {
            Root::Stage(stage) => TemplateError::UnresolvedField {
                placeholder: reference.placeholder.clone(),
                stage: stage.clone(),
                field: reference.path[..=depth].join("."),
            },
            Root::Task => TemplateError::UnresolvedTaskField {
                placeholder: reference.placeholder.clone(),
                field: reference.path[..=depth].join("."),
            },
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

    /// Renders `input` and returns just the text, for the (common) tests
    /// that don't care about `render`'s `unresolved` list.
    fn render_text(input: &str, payload: &Value) -> String {
        render(input, payload).unwrap().0
    }

    #[test]
    fn renders_a_single_field() {
        let rendered = render_text("gh pr checks {{ stages.open_pr.number }}", &payload());
        assert_eq!(rendered, "gh pr checks 42");
    }

    #[test]
    fn renders_a_nested_path() {
        let rendered = render_text("{{ stages.review.detail.note }}", &payload());
        assert_eq!(rendered, "ship it");
    }

    #[test]
    fn renders_a_bare_stage_reference_for_a_text_capture() {
        let rendered = render_text("state={{ stages.checks }}", &payload());
        assert_eq!(rendered, "state=SUCCESS");
    }

    #[test]
    fn tolerates_whitespace_around_the_path() {
        let payload = payload();
        for input in [
            "{{stages.open_pr.number}}",
            "{{ stages.open_pr.number }}",
            "{{      stages.open_pr.number   }}",
            "{{\tstages.open_pr.number\n}}",
        ] {
            assert_eq!(render_text(input, &payload), "42", "for {input:?}");
        }
    }

    /// This is a hand-written scanner over byte indices, so multi-byte text
    /// around and inside a placeholder is the standing panic risk. `{`/`}`
    /// can't appear inside a UTF-8 continuation byte, so every index it
    /// derives is a char boundary — this is the guard against a refactor
    /// quietly breaking that.
    #[test]
    fn handles_multibyte_text_around_and_inside_placeholders() {
        let payload = json!({
            "stages": { "review": { "comments": "见 café ☕", "n": 1 } }
        });
        assert_eq!(
            render_text("café {{ stages.review.comments }} 🚀", &payload),
            "café 见 café ☕ 🚀"
        );
        // Multi-byte in the *unresolved* and *error* paths too, where the
        // placeholder text is sliced out of the input to quote back.
        let (rendered, unresolved) = render("🚀 {{ stages.review.née }}", &payload).unwrap();
        assert_eq!(rendered, "🚀 ");
        assert_eq!(unresolved, vec!["{{ stages.review.née }}"]);
        let (rendered, unresolved) = render("🚀 {{ stages.☕.x }}", &payload).unwrap();
        assert_eq!(rendered, "🚀 ");
        assert_eq!(unresolved, vec!["{{ stages.☕.x }}"]);
        // Malformed syntax stays a hard error regardless.
        assert!(render("🚀 {{ stages.review", &payload).is_err());
        assert!(render("🚀 {{ 見.x }}", &payload).is_err());
    }

    #[test]
    fn handles_adjacent_and_nested_looking_placeholders() {
        let payload = json!({"stages": {"a": {"x": 1}, "b": {"y": 2}}});
        assert_eq!(
            render_text("{{ stages.a.x }}{{ stages.b.y }}", &payload),
            "12"
        );
        // A `}}` with no opener is literal text, not an error.
        assert_eq!(render_text("}} plain", &payload), "}} plain");
        // An inner `{{` is malformed syntax (whitespace inside the path),
        // not a missing value — still a hard error.
        assert!(render("{{ {{ stages.a.x }} }}", &payload).is_err());
    }

    #[test]
    fn renders_several_placeholders_in_one_string() {
        let rendered = render_text(
            "pr {{ stages.open_pr.number }} at {{ stages.open_pr.url }} is {{ stages.review.outcome }}",
            &payload(),
        );
        assert_eq!(rendered, "pr 42 at https://example.test/pr/42 is approved");
    }

    #[test]
    fn renders_booleans_and_numbers_as_plain_scalars() {
        assert_eq!(
            render_text("{{ stages.open_pr.draft }}", &payload()),
            "true"
        );
        assert_eq!(render_text("{{ stages.open_pr.number }}", &payload()), "42");
    }

    #[test]
    fn leaves_text_without_placeholders_untouched() {
        let input = "gh pr create --fill { not a placeholder }";
        assert_eq!(render_text(input, &json!({})), input);
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
    fn an_unknown_root_is_rejected() {
        let err = render("{{ bogus.id }}", &payload()).unwrap_err();
        assert!(
            matches!(&err, TemplateError::UnknownNamespace { root, .. } if root == "bogus"),
            "{err}"
        );
    }

    fn task_payload() -> Value {
        json!({ "task": { "input": "fix the flaky test", "title": "Investigate CI" } })
    }

    #[test]
    fn resolves_task_input_and_title() {
        let rendered = render_text("Task: {{ task.title }}\n{{ task.input }}", &task_payload());
        assert_eq!(rendered, "Task: Investigate CI\nfix the flaky test");
    }

    /// #60: a missing *value* — as opposed to malformed syntax — renders as
    /// an empty string and is reported back via `unresolved`, rather than
    /// failing the whole render. `TemplateError::is_missing_value` is what
    /// draws this line; these eight tests pin every variant it covers.
    #[test]
    fn an_unknown_task_field_renders_empty_and_is_noted() {
        let (rendered, unresolved) = render("{{ task.id }}", &task_payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ task.id }}"]);
    }

    #[test]
    fn a_task_field_renders_empty_when_the_payload_has_no_task_key_at_all() {
        // e.g. a row from before `start_task` started seeding `payload.task`.
        let (rendered, unresolved) = render("{{ task.input }}", &json!({})).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ task.input }}"]);
    }

    #[test]
    fn a_bare_task_reference_renders_empty() {
        let (rendered, unresolved) = render("{{ task }}", &task_payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ task }}"]);
    }

    #[test]
    fn a_stage_that_has_captured_nothing_renders_empty() {
        let (rendered, unresolved) = render("{{ stages.never_ran.url }}", &payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ stages.never_ran.url }}"]);
    }

    #[test]
    fn a_missing_field_renders_empty_and_is_noted_with_the_full_path() {
        let (rendered, unresolved) =
            render("{{ stages.review.detail.missing }}", &payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ stages.review.detail.missing }}"]);
    }

    #[test]
    fn indexing_into_a_text_capture_renders_empty_rather_than_a_panic() {
        let (rendered, unresolved) = render("{{ stages.checks.state }}", &payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ stages.checks.state }}"]);
    }

    #[test]
    fn an_object_renders_empty() {
        let (rendered, unresolved) = render("{{ stages.open_pr }}", &payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ stages.open_pr }}"]);
    }

    #[test]
    fn a_null_capture_renders_empty() {
        let (rendered, unresolved) = render("{{ stages.empty.value }}", &payload()).unwrap();
        assert_eq!(rendered, "");
        assert_eq!(unresolved, vec!["{{ stages.empty.value }}"]);
    }

    #[test]
    fn a_missing_value_among_resolved_literals_only_blanks_its_own_placeholder() {
        let (rendered, unresolved) = render(
            "pr {{ stages.open_pr.number }} status={{ stages.open_pr.missing }} done",
            &payload(),
        )
        .unwrap();
        assert_eq!(rendered, "pr 42 status= done");
        assert_eq!(unresolved, vec!["{{ stages.open_pr.missing }}"]);
    }

    #[test]
    fn references_lists_every_placeholder_in_order() {
        let found =
            references("a {{ stages.one.x }} b {{ stages.two }} c {{ task.input }}").unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].root, Root::Stage("one".to_string()));
        assert_eq!(found[0].path, vec!["x".to_string()]);
        assert_eq!(found[1].root, Root::Stage("two".to_string()));
        assert!(found[1].path.is_empty());
        assert_eq!(found[2].root, Root::Task);
        assert_eq!(found[2].path, vec!["input".to_string()]);
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
