//! Loading `kind: open_review`: a shell stage that publishes the task's review.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;

fn dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "chocofactoryd-open-review-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

/// A workflow whose stage `publish` has `body` (indented under it), then a terminal.
fn flow(body: &str) -> String {
    format!(
        "name: review\nstages:\n  publish:\n    kind: open_review\n{body}  finished:\n    kind: terminal\n"
    )
}

const GOOD: &str = "    command: \"true\"\n    on: { done: finished }\n";

fn parse(yaml: &str, dir: &Path) -> Result<WorkflowDefinition, WorkflowDefError> {
    WorkflowDefinition::parse(yaml, dir)
}

fn err_of(body: &str) -> WorkflowDefError {
    parse(&flow(body), &dir()).unwrap_err()
}

#[test]
fn a_publisher_resolves_to_a_shell_stage_with_an_implied_json_capture() {
    let def = parse(&flow(GOOD), &dir()).unwrap();
    assert!(matches!(
        &def.stages["publish"].kind,
        StageKind::Shell {
            capture: Some(Capture::Json),
            publishes_review: true,
            ..
        }
    ));
    assert_eq!(def.stages["publish"].kind.name(), "open_review");

    // An ordinary shell stage is not a publisher, whatever its capture.
    let yaml = "name: x\nstages:\n  run:\n    kind: shell\n    command: \"true\"\n    capture: json\n    on: { done: finished }\n  finished:\n    kind: terminal\n";
    let def = parse(yaml, &dir()).unwrap();
    assert!(matches!(
        &def.stages["run"].kind,
        StageKind::Shell {
            publishes_review: false,
            ..
        }
    ));
    assert_eq!(def.stages["run"].kind.name(), "shell");
}

#[test]
fn all_seven_yaml_kinds_report_their_yaml_names() {
    let dir = dir();
    fs::write(dir.join("turn.md"), "x").unwrap();
    let yaml = r#"
name: names
roles:
  coder: { cli: claude, model: sonnet, read_only: true, disallowed_tools: [edit, write, notebook_edit] }
worktree: true
stages:
  a: { kind: agent_turn, role: coder, prompt_file: turn.md, on: { done: b } }
  b: { kind: shell, command: "true", on: { done: c } }
  c: { kind: open_review, command: "true", on: { done: d } }
  d: { kind: poll, command: "true", interval: 1s, outcomes: [{ match: x, then: done }], on: { done: e } }
  e: { kind: human_gate, on: { resumed: f } }
  f:
    kind: parallel
    branches:
      one: { kind: agent_turn, role: coder, prompt_file: turn.md }
      two: { kind: agent_turn, role: coder, prompt_file: turn.md }
    on: { done: g }
  g: { kind: terminal }
"#;
    let def = parse(yaml, &dir).unwrap();
    let names: Vec<_> = def.stages.values().map(|s| s.kind.name()).collect();
    assert_eq!(
        names,
        [
            "agent_turn",
            "shell",
            "open_review",
            "poll",
            "human_gate",
            "parallel",
            "terminal"
        ]
    );
}

#[test]
fn two_publishers_are_refused_naming_both_in_declaration_order() {
    // Declared second-then-first alphabetically, so a sorted or reversed
    // report would name them the wrong way round.
    let yaml = "name: x\nstages:\n  zeta:\n    kind: open_review\n    command: \"true\"\n    on: { done: alpha }\n  alpha:\n    kind: open_review\n    command: \"true\"\n    on: { done: finished }\n  finished:\n    kind: terminal\n";
    let err = parse(yaml, &dir()).unwrap_err();
    assert!(
        matches!(&err, WorkflowDefError::MultipleReviewPublishers { first, second }
            if first == "zeta" && second == "alpha"),
        "{err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("more than one open_review stage")
            && message.contains("'zeta'")
            && message.contains("'alpha'"),
        "{message}"
    );
}

#[test]
fn a_publisher_is_never_a_parallel_branch_but_shell_and_poll_are_only_not_yet() {
    let group = |branch: &str| {
        format!(
            "name: x\nworktree: true\nroles:\n  r: {{ cli: claude, model: opus, read_only: true, disallowed_tools: [edit, write, notebook_edit] }}\nstages:\n  g:\n    kind: parallel\n    branches:\n      one: {{ kind: agent_turn, role: r }}\n      two:\n        {branch}\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
        )
    };
    let err = parse(
        &group("kind: open_review\n        command: \"true\""),
        &dir(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, WorkflowDefError::BranchKindNeverAllowed { group, branch, kind }
            if group == "g" && branch == "two" && *kind == "open_review"),
        "{err:?}"
    );
    let err = parse(&group("kind: shell\n        command: \"true\""), &dir()).unwrap_err();
    assert!(
        matches!(&err, WorkflowDefError::BranchKindNotYetSupported { kind, .. } if *kind == "shell"),
        "{err:?}"
    );
}

#[test]
fn an_explicit_capture_key_is_refused_even_when_null() {
    for capture in ["json", "text", "null", "~"] {
        let err = err_of(&format!(
            "    command: \"true\"\n    capture: {capture}\n    on: {{ done: finished }}\n"
        ));
        assert!(
            matches!(&err, WorkflowDefError::UnknownStageKey { stage, key }
                if stage == "publish" && key == "capture"),
            "capture: {capture}: {err:?}"
        );
    }
}

#[test]
fn a_misspelled_publisher_key_is_refused() {
    let err = err_of("    command: \"true\"\n    timeuot: 5s\n    on: { done: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::UnknownStageKey { stage, key }
            if stage == "publish" && key == "timeuot"),
        "{err:?}"
    );
}

#[test]
fn a_publisher_needs_a_done_edge() {
    let err = err_of("    command: \"true\"\n    on: { error: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::MissingShellDoneOutcome { stage } if stage == "publish"),
        "{err:?}"
    );
}

#[test]
fn a_publisher_needs_exactly_one_of_command_and_script_file() {
    let err = err_of("    on: { done: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::MissingShellCommand { stage } if stage == "publish"),
        "{err:?}"
    );
    let dir = dir();
    crate::test_support::write_executable(&dir.join("p.sh"), "#!/bin/sh\n");
    let err = parse(
        &flow("    command: \"true\"\n    script_file: p.sh\n    on: { done: finished }\n"),
        &dir,
    )
    .unwrap_err();
    assert!(
        matches!(&err, WorkflowDefError::AmbiguousShellCommand { stage } if stage == "publish"),
        "{err:?}"
    );
}

#[test]
fn a_publisher_shares_the_shell_reference_duration_env_and_template_checks() {
    // A script_file that is missing.
    let err = err_of("    script_file: nowhere.sh\n    on: { done: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::MissingReferencedFile { field, .. } if *field == "script_file"),
        "{err:?}"
    );
    // A script_file that is deleted after being written.
    let dir = dir();
    let script = dir.join("p.sh");
    crate::test_support::write_executable(&script, "#!/bin/sh\n");
    fs::remove_file(&script).unwrap();
    let err = parse(
        &flow("    script_file: p.sh\n    on: { done: finished }\n"),
        &dir,
    )
    .unwrap_err();
    assert!(
        matches!(&err, WorkflowDefError::MissingReferencedFile { .. }),
        "{err:?}"
    );
    // Duration, env name, duplicate env key, unknown stage template.
    let err = err_of("    command: \"true\"\n    timeout: soon\n    on: { done: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::InvalidDuration { stage, field, .. } if stage == "publish" && *field == "timeout"),
        "{err:?}"
    );
    let err = err_of("    command: \"true\"\n    env: { \"1X\": v }\n    on: { done: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::InvalidEnvName { stage, name } if stage == "publish" && name == "1X"),
        "{err:?}"
    );
    let err = parse(
        &flow("    command: \"true\"\n    env:\n      A: one\n      A: two\n    on: { done: finished }\n"),
        &dir,
    )
    .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("duplicate"),
        "{err}"
    );
    let err =
        err_of("    command: \"echo {{ stages.ghost.number }}\"\n    on: { done: finished }\n");
    assert!(
        matches!(&err, WorkflowDefError::UnknownTemplateStage { stage, referenced, .. }
            if stage == "publish" && referenced == "ghost"),
        "{err:?}"
    );
}

#[test]
fn a_later_stage_may_template_the_publishers_fields() {
    let yaml = "name: x\nstages:\n  publish:\n    kind: open_review\n    command: \"true\"\n    on: { done: after }\n  after:\n    kind: shell\n    command: \"echo {{ stages.publish.number }}\"\n    env: { L: \"{{ stages.publish.label }}\" }\n    on: { done: finished }\n  finished:\n    kind: terminal\n";
    parse(yaml, &dir()).unwrap();
}
