//! `human_gate` stages: entering one (with or without a watcher) and
//! answering one with a reply through choco (#59, #175).

use super::stage_capture::derive_capture;
use super::*;
use crate::workflow_def::ReplyMarker;

/// The outcome a reply chose, and what is left of the reply once the marker
/// lines are taken out.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReplyVerdict<'a> {
    pub outcome: &'a str,
    pub review: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReplyMarkerError {
    /// No line of the reply is a marker.
    NoMarker,
    /// Markers for different outcomes. `found` lists the distinct marker
    /// lines present, in the order the gate declares them.
    Conflict { found: Vec<String> },
}

/// Reads a reply's verdict from its marker lines.
///
/// This is the same line rule as the `VERDICT` filter in
/// `workflows/scripts/await-review.sh`: a line counts when, with trailing
/// spaces, tabs and `\r` removed, it equals a marker exactly (case-sensitive,
/// leading whitespace is not ignored). The case table
/// `chocofactoryd/tests/fixtures/review-markers.json` defines it, and both
/// implementations are tested against that table.
///
/// Where they differ: a comment with two markers resolves to
/// `REQUEST_CHANGES` on GitHub, which cannot refuse a posted comment, while a
/// reply is refused (`Conflict`). The same marker twice is one outcome.
///
/// `review` is the reply with every marker line removed and the other lines
/// kept exactly as typed, minus leading and trailing blank lines.
pub(crate) fn reply_verdict<'a>(
    text: &str,
    markers: &'a [ReplyMarker],
) -> Result<ReplyVerdict<'a>, ReplyMarkerError> {
    let strip = |line: &str| line.trim_end_matches([' ', '\t', '\r']).to_string();
    let mut found: Vec<&'a ReplyMarker> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let stripped = strip(line);
        match markers.iter().find(|m| m.line == stripped) {
            Some(marker) => {
                if !found.iter().any(|f| f.line == marker.line) {
                    found.push(marker);
                }
            }
            None => kept.push(line),
        }
    }

    let mut outcomes: Vec<&'a str> = Vec::new();
    for marker in &found {
        if !outcomes.contains(&marker.then.as_str()) {
            outcomes.push(marker.then.as_str());
        }
    }
    let outcome = match outcomes.as_slice() {
        [] => return Err(ReplyMarkerError::NoMarker),
        [one] => *one,
        _ => {
            let found = markers
                .iter()
                .filter(|m| found.iter().any(|f| f.line == m.line))
                .map(|m| m.line.clone())
                .collect();
            return Err(ReplyMarkerError::Conflict { found });
        }
    };

    let is_blank = |line: &&str| strip(line).is_empty();
    let start = kept.iter().position(|l| !is_blank(l)).unwrap_or(kept.len());
    let end = kept
        .iter()
        .rposition(|l| !is_blank(l))
        .map_or(start, |i| i + 1);
    Ok(ReplyVerdict {
        outcome,
        review: kept[start..end].join("\n"),
    })
}

impl WorkflowEngine {
    /// Enters a `human_gate`: pauses the task for a person, and when the
    /// gate declares a `watch:` starts that watcher too.
    pub(super) async fn enter_gate(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageKind::HumanGate { capture, .. } = &entry.stage_def.kind else {
            unreachable!("enter_gate is only called for HumanGate stages")
        };
        match entry.stage_def.watch() {
            Some(watch) => self.start_watch(entry, watch, *capture).await,
            None => Ok(()),
        }
    }

    /// A reply through choco to the gate `gate` the task is waiting at.
    ///
    /// Without `markers:` the reply is the resume signal itself: outcome
    /// `resumed`, the text captured verbatim (#59). With them, the reply must
    /// carry a marker line, which picks the outcome, and the capture is the
    /// reply without its marker lines. A refused reply returns before
    /// anything is recorded or written, and leaves the watcher running.
    pub(super) async fn reply_to_gate(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        gate: &str,
        text: &str,
    ) -> Result<(), SendMessageOrResumeError> {
        let StageKind::HumanGate {
            capture, markers, ..
        } = &definition.stages[gate].kind
        else {
            return Err(SendMessageOrResumeError::UnsupportedStageKind(
                gate.to_string(),
            ));
        };

        let (outcome, captured, note) = if markers.is_empty() {
            let (captured, note) =
                derive_capture(*capture, text, task_id, gate, "the human's message");
            ("resumed", captured, note)
        } else {
            let verdict = match reply_verdict(text, markers) {
                Ok(verdict) => verdict,
                Err(ReplyMarkerError::NoMarker) => {
                    return Err(SendMessageOrResumeError::ReplyNeedsMarker {
                        stage: gate.to_string(),
                        markers: markers.iter().map(|m| m.line.clone()).collect(),
                    });
                }
                Err(ReplyMarkerError::Conflict { found }) => {
                    return Err(SendMessageOrResumeError::ReplyHasConflictingMarkers {
                        stage: gate.to_string(),
                        found,
                    });
                }
            };
            let (captured, note) = derive_capture(
                *capture,
                &verdict.review,
                task_id,
                gate,
                "the human's message",
            );
            (verdict.outcome, captured, note)
        };

        // Best-effort, log-and-continue — same as `send_message`'s
        // chat-path recording. No `session` exists for a `human_gate`, so
        // this uses `append_for_task`, the task-scoped, session-less path
        // `dispatch_stage` already uses for `StageEntered`/`Error`. The
        // event records what the reply asked for, in the text as typed.
        let mut payload = json!({ "text": text, "outcome": outcome });
        if let Some(note) = note {
            payload["note"] = json!(note);
        }
        match events::append_for_task(&self.pool, task_id, EventType::HumanMessage, payload).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => {
                tracing::error!(task_id, %err, "failed to record human message event")
            }
        }

        // `stop_watcher: true` stops the gate's watcher (if any) inside
        // `advance_from_stage`, under the task lock, as the last step before
        // the state write.
        match self
            .advance_from_stage(task_id, definition, outcome, Some(gate), captured, true)
            .await
        {
            Ok(()) => Ok(()),
            // The benign races: another caller already resumed or
            // cancelled this task, so there's nothing to rescue.
            // `is_benign_resume_race` is also what `api/error.rs`'s
            // `SendMessageOrResumeError` → `ApiError` mapping calls,
            // which maps these same variants to 409 for exactly this
            // reason — see the comments there.
            Err(err) if err.is_benign_resume_race() => Err(SendMessageOrResumeError::Advance(err)),
            // Anything else — a session that won't spawn, a prompt
            // template that fails to render, a transient DB error —
            // and the gate has already committed
            // `workflow_state.current_stage` to the next stage
            // before failing to enter it (see `stage_to_blame`'s
            // doc comment), so the task is left `open` with nothing
            // running unless this marks it `stuck`. Mirrors the
            // catch-alls in `finish_detached`/
            // `finish_turn` — the human-gate path is the one
            // #61 left without one.
            Err(err) => {
                let current_stage = gate;
                tracing::error!(
                    task_id, stage = current_stage, %err,
                    "task wedged: its human_gate was resumed but the transition failed"
                );
                let blamed = self.stage_to_blame(task_id, current_stage).await;
                // `blamed == current_stage` means the failure happened
                // before `workflow_state::update` committed the next
                // stage — e.g. a DB error updating state inside
                // `advance_from_stage` itself. Covered by
                // `resuming_a_human_gate_whose_own_transition_fails_marks_the_task_stuck_at_the_gate`,
                // which injects exactly that with a SQLite trigger
                // on `workflow_state`.
                let reason = if blamed == current_stage {
                    format!("stage '{current_stage}': resumed but the transition failed: {err}")
                } else {
                    format!(
                        "stage '{blamed}': could not be entered after '{current_stage}' \
                         was resumed: {err}"
                    )
                };
                // `enter_stage` already appends its own `Error`
                // event for a template failure — see `mark_stuck`'s
                // doc comment.
                self.mark_stuck(
                    task_id,
                    &reason,
                    matches!(err, EngineError::Template { .. }),
                )
                .await;
                Err(SendMessageOrResumeError::Advance(err))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markers() -> Vec<ReplyMarker> {
        vec![
            ReplyMarker {
                line: "/request-changes".into(),
                then: "changes_requested".into(),
            },
            ReplyMarker {
                line: "/approve".into(),
                then: "approved".into(),
            },
        ]
    }

    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        body: String,
        choco: String,
    }

    fn cases() -> Vec<Case> {
        serde_json::from_str(include_str!("../../tests/fixtures/review-markers.json")).unwrap()
    }

    #[test]
    fn reply_verdict_matches_the_shared_case_table() {
        let markers = markers();
        let cases = cases();
        assert!(cases.len() >= 16);
        for case in cases {
            let got = crate::engine::reply_verdict(&case.body, &markers);
            let label = match &got {
                Ok(v) => v.outcome.to_string(),
                Err(ReplyMarkerError::NoMarker) => "refused_no_marker".to_string(),
                Err(ReplyMarkerError::Conflict { .. }) => "refused_conflict".to_string(),
            };
            assert_eq!(label, case.choco, "case '{}'", case.name);
            if let Ok(v) = got {
                for line in v.review.split('\n') {
                    let stripped = line.trim_end_matches([' ', '\t', '\r']);
                    assert!(
                        !markers.iter().any(|m| m.line == stripped),
                        "case '{}': review still has a marker line {line:?}",
                        case.name
                    );
                }
            }
        }
    }

    #[test]
    fn review_text_drops_markers_and_edge_blank_lines() {
        let markers = markers();
        let by_name = |name: &str| {
            cases()
                .into_iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no case '{name}'"))
        };
        let v = crate::engine::reply_verdict(&by_name("prose then marker").body, &markers).unwrap();
        assert_eq!(v.review, "Two things.");
        let v = crate::engine::reply_verdict(&by_name("bare approve").body, &markers).unwrap();
        assert_eq!(v.review, "");
    }

    #[test]
    fn review_keeps_other_lines_exactly_as_typed() {
        let markers = markers();
        let v = crate::engine::reply_verdict("\n  a \r\n\n/approve\n b\t\n\n", &markers).unwrap();
        assert_eq!(v.review, "  a \r\n\n b\t");
    }

    #[test]
    fn conflict_lists_the_found_lines_in_marker_order() {
        let markers = markers();
        let by = cases()
            .into_iter()
            .find(|c| c.name == "both markers")
            .unwrap();
        assert_eq!(
            crate::engine::reply_verdict(&by.body, &markers),
            Err(ReplyMarkerError::Conflict {
                found: vec!["/request-changes".into(), "/approve".into()]
            })
        );
    }

    #[test]
    fn two_lines_for_one_outcome_are_not_a_conflict() {
        let markers = vec![
            ReplyMarker {
                line: "/ok".into(),
                then: "approved".into(),
            },
            ReplyMarker {
                line: "/approve".into(),
                then: "approved".into(),
            },
        ];
        let v = crate::engine::reply_verdict("/ok\n/approve", &markers).unwrap();
        assert_eq!(v.outcome, "approved");
    }
}
