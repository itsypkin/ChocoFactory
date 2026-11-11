//! Report selection, the first-failure latch and the cutoff tracker: what
//! counts as a completed single-shot turn, and what makes one unable to
//! complete. Unit tests of [`SingleShotTurn`] first, then whole drains over
//! scripted fake CLIs, with real SQLite triggers and locks standing in for a
//! database that cannot record what it is given.
//!
//! The ids are production-shaped and distinct per logical call. A success
//! has no `is_error` and array content; a rejection has `is_error: true`
//! and string content.

use super::*;
use crate::adapter::{BillingMode, TokenCounts, TurnUsage, UsageCounting};

const CALL_A: &str = REPORT_CALL_ID;
const CALL_B: &str = "toolu_01B7mQ2xKd9PzRtHs4VnWcLe";
const CALL_C: &str = "toolu_01C5nR3yLe8QaSuIt5WoXdMf";
const HELPER: &str = "toolu_01W3VQxTmyXy3B7b1bmz9YaX";

// ---------------------------------------------------------------- helpers

fn tool() -> String {
    chocofactory_core::mcp::qualified_report_outcome_tool_name()
}

fn usage() -> TurnUsage {
    TurnUsage {
        cost_usd: Some(0.01),
        tokens: TokenCounts {
            input: Some(3),
            output: Some(9),
            cache_read: None,
            cache_write: None,
        },
        models: None,
        wall_time_ms: Some(1200),
        model_turns: Some(2),
        billing: BillingMode::Subscription,
        counting: UsageCounting::PerTurn,
    }
}

fn call(id: &str, outcome: &str) -> AgentEvent {
    AgentEvent::ToolCall {
        tool_use_id: id.to_string(),
        tool: tool(),
        input: json!({"outcome": outcome, "summary": ""}),
    }
}

fn verdict(id: &str, rejected: bool) -> AgentEvent {
    AgentEvent::ToolResult {
        tool_use_id: id.to_string(),
        tool: tool(),
        output: "verdict".to_string(),
        is_error: rejected,
    }
}

fn end(is_error: bool) -> AgentEvent {
    AgentEvent::TurnCompleted {
        is_error,
        usage: usage(),
    }
}

fn diagnostic(message: &str) -> AgentEvent {
    AgentEvent::ProtocolError {
        message: message.to_string(),
    }
}

fn wrapped(event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: HELPER.to_string(),
        event: Box::new(event),
    }
}

fn interrupted() -> AgentEvent {
    AgentEvent::Interrupted {
        message: "limit".to_string(),
        detected_by: crate::adapter::InterruptionEvidence::Structured,
    }
}

fn text() -> AgentEvent {
    AgentEvent::AssistantMessage {
        text: "working".to_string(),
    }
}

fn feed(turn: &mut SingleShotTurn, events: impl IntoIterator<Item = AgentEvent>) -> Vec<TurnStep> {
    events.into_iter().map(|e| turn.observe(&e)).collect()
}

/// An open turn that has accepted `id`.
fn with_accepted(id: &str) -> SingleShotTurn {
    let mut turn = SingleShotTurn::default();
    feed(&mut turn, [call(id, "approved"), verdict(id, false)]);
    turn
}

// ------------------------------------------------------- unit: selection

#[test]
fn la1_the_newest_accepted_report_is_selected_and_replays_change_nothing() {
    let mut turn = SingleShotTurn::default();
    feed(
        &mut turn,
        [
            call(CALL_A, "approved"),
            verdict(CALL_A, false),
            call(CALL_B, "changes_requested"),
            verdict(CALL_B, false),
        ],
    );
    assert_eq!(turn.selected_report.as_deref(), Some(CALL_B));

    // Identical replays of A's call and result: no change, no re-selection.
    let steps = feed(
        &mut turn,
        [call(CALL_A, "approved"), verdict(CALL_A, false)],
    );
    assert_eq!(steps, vec![TurnStep::Continue, TurnStep::Continue]);
    assert_eq!(
        turn.selected_report.as_deref(),
        Some(CALL_B),
        "selected_report == B"
    );
    assert!(turn.failure.is_none());
    assert_eq!(
        turn.report_calls.get(CALL_A),
        Some(&json!({"outcome": "approved", "summary": ""}))
    );

    assert_eq!(turn.observe(&end(false)), TurnStep::Completed);
    assert!(turn.completed);
    assert_eq!(turn.completed_report.as_deref(), Some(CALL_B));
}

#[test]
fn acceptance_order_is_the_order_the_acceptances_were_seen_not_the_order_of_the_calls() {
    let mut turn = SingleShotTurn::default();
    feed(
        &mut turn,
        [
            call(CALL_A, "approved"),
            call(CALL_B, "changes_requested"),
            verdict(CALL_B, false),
            verdict(CALL_A, false),
        ],
    );
    assert_eq!(turn.selected_report.as_deref(), Some(CALL_A));
}

#[test]
fn a_rejection_of_another_report_keeps_the_accepted_selection() {
    let mut turn = with_accepted(CALL_A);
    feed(&mut turn, [call(CALL_B, "bogus"), verdict(CALL_B, true)]);
    assert_eq!(turn.selected_report.as_deref(), Some(CALL_A));
    assert_eq!(turn.report_results.get(CALL_B), Some(&false));
    assert!(turn.failure.is_none());
    assert_eq!(turn.observe(&end(false)), TurnStep::Completed);
    assert_eq!(turn.completed_report.as_deref(), Some(CALL_A));
}

#[test]
fn a_result_is_tied_to_a_call_only_if_the_call_came_first() {
    let mut turn = SingleShotTurn::default();
    // The result arrives before its call: not report evidence, and the
    // call that follows is not retroactively associated with it.
    feed(
        &mut turn,
        [verdict(CALL_A, false), call(CALL_A, "approved")],
    );
    assert!(turn.report_results.is_empty());
    assert!(turn.selected_report.is_none());
    assert_eq!(turn.observe(&end(false)), TurnStep::WaitingForReport);
}

#[test]
fn only_the_main_agents_report_counts() {
    let mut turn = SingleShotTurn::default();
    feed(
        &mut turn,
        [
            wrapped(call(CALL_A, "approved")),
            wrapped(verdict(CALL_A, false)),
        ],
    );
    assert!(turn.report_calls.is_empty());
    assert!(turn.selected_report.is_none());
    // A non-report tool's call and result are not tracked either.
    feed(
        &mut turn,
        [
            AgentEvent::ToolCall {
                tool_use_id: CALL_B.to_string(),
                tool: "Bash".to_string(),
                input: json!({"command": "ls"}),
            },
            AgentEvent::ToolResult {
                tool_use_id: CALL_B.to_string(),
                tool: "Bash".to_string(),
                output: String::new(),
                is_error: false,
            },
        ],
    );
    assert!(turn.report_calls.is_empty() && turn.report_results.is_empty());
}

#[test]
fn an_accepted_report_after_a_reportless_end_needs_a_later_clean_end() {
    let mut turn = SingleShotTurn::default();
    assert_eq!(turn.observe(&end(false)), TurnStep::WaitingForReport);
    let steps = feed(
        &mut turn,
        [call(CALL_A, "approved"), verdict(CALL_A, false)],
    );
    assert_eq!(steps, vec![TurnStep::Continue, TurnStep::Continue]);
    assert!(
        !turn.completed,
        "status is not Idle: the report alone is not an end"
    );
    assert!(!turn.waiting_for_report, "the agent is working again");
    assert_eq!(turn.observe(&end(false)), TurnStep::Completed);
}

#[test]
fn an_error_end_after_an_accepted_report_does_not_complete() {
    let mut turn = with_accepted(CALL_A);
    assert_eq!(turn.observe(&end(true)), TurnStep::EndedWithError);
    assert!(turn.errored && !turn.completed);
    assert!(turn.completed_report.is_none());
}

// LA4
#[test]
fn la4_nothing_after_completion_changes_the_selection_or_the_failure() {
    let mut turn = with_accepted(CALL_A);
    assert_eq!(turn.observe(&end(false)), TurnStep::Completed);
    let steps = feed(
        &mut turn,
        [
            call(CALL_C, "approved"),
            verdict(CALL_C, false),
            call(CALL_A, "something else"),
            verdict(CALL_A, true),
            diagnostic("malformed CLI output: late"),
            end(false),
            end(true),
        ],
    );
    assert_eq!(
        turn.selected_report.as_deref(),
        Some(CALL_A),
        "selected_report unchanged"
    );
    assert_eq!(turn.completed_report.as_deref(), Some(CALL_A));
    assert!(turn.failure.is_none());
    assert!(!turn.report_calls.contains_key(CALL_C));
    assert!(steps.iter().all(|s| *s == TurnStep::Continue), "{steps:?}");
}

// ----------------------------------------------- unit: the failure latch

#[test]
fn a_call_repeated_with_different_input_latches_with_the_exact_message() {
    let mut turn = with_accepted(CALL_A);
    let step = turn.observe(&call(CALL_A, "changes_requested"));
    let message = format!(
        "report_outcome call '{CALL_A}' was repeated with different input; this turn cannot complete"
    );
    assert_eq!(
        step,
        TurnStep::EvidenceFailed {
            diagnostic: Some(message.clone())
        }
    );
    assert_eq!(turn.failure, Some(message));
    assert!(turn.errored && !turn.waiting_for_report);
    assert_eq!(
        turn.report_calls.get(CALL_A),
        Some(&json!({"outcome": "approved", "summary": ""})),
        "the original input is kept"
    );
    // The latch is a stop: the clean end after it does not complete.
    assert_eq!(turn.observe(&end(false)), TurnStep::Continue);
    assert!(!turn.completed);
}

#[test]
fn replayed_inputs_are_compared_as_json_values_whatever_their_shape() {
    let shapes = [
        json!({"outcome": "done", "extra": {"nested": [1, 2, {"k": null}]}}),
        json!(null),
        json!("a string"),
        json!(["a", 1]),
        json!(7),
    ];
    for input in shapes {
        let mut turn = SingleShotTurn::default();
        let event = |input: &Value| AgentEvent::ToolCall {
            tool_use_id: CALL_A.to_string(),
            tool: tool(),
            input: input.clone(),
        };
        assert_eq!(turn.observe(&event(&input)), TurnStep::Continue);
        assert_eq!(
            turn.observe(&event(&input)),
            TurnStep::Continue,
            "an identical replay of {input} is not a conflict"
        );
        assert!(turn.failure.is_none());
        let different = match &input {
            Value::Null => json!({}),
            _ => Value::Null,
        };
        assert!(
            matches!(
                turn.observe(&event(&different)),
                TurnStep::EvidenceFailed { .. }
            ),
            "{input} then {different} is a conflict"
        );
    }
}

#[test]
fn a_changed_verdict_latches_with_chronological_wording() {
    let mut turn = with_accepted(CALL_A);
    let step = turn.observe(&verdict(CALL_A, true));
    let message = format!(
        "report_outcome call '{CALL_A}' got a rejected result after a successful result; \
         this turn cannot complete"
    );
    assert_eq!(
        step,
        TurnStep::EvidenceFailed {
            diagnostic: Some(message.clone())
        }
    );
    assert_eq!(turn.failure, Some(message));
    assert_eq!(
        turn.report_results.get(CALL_A),
        Some(&true),
        "first verdict kept"
    );

    let mut turn = SingleShotTurn::default();
    feed(&mut turn, [call(CALL_A, "approved"), verdict(CALL_A, true)]);
    let step = turn.observe(&verdict(CALL_A, false));
    assert_eq!(
        step,
        TurnStep::EvidenceFailed {
            diagnostic: Some(format!(
                "report_outcome call '{CALL_A}' got a successful result after a rejected \
                 result; this turn cannot complete"
            ))
        }
    );
    assert!(
        turn.selected_report.is_none(),
        "the late acceptance selects nothing"
    );
}

#[test]
fn a_repeated_same_verdict_changes_nothing_and_does_not_reselect() {
    let mut turn = SingleShotTurn::default();
    feed(
        &mut turn,
        [
            call(CALL_A, "approved"),
            verdict(CALL_A, false),
            call(CALL_B, "changes_requested"),
            verdict(CALL_B, false),
            verdict(CALL_A, false),
        ],
    );
    assert_eq!(turn.selected_report.as_deref(), Some(CALL_B));
    assert!(turn.failure.is_none());
}

#[test]
fn only_the_first_failure_is_kept() {
    let mut turn = SingleShotTurn::default();
    assert!(turn.latch_failure("first".to_string()));
    assert!(!turn.latch_failure("second".to_string()));
    assert_eq!(
        turn.failure.as_deref(),
        Some("first"),
        "first failure retained"
    );
}

#[test]
fn a_latch_on_a_turn_with_a_grace_deadline_keeps_it_and_changes_its_cause() {
    let mut turn = SingleShotTurn::default();
    turn.arm_grace(StdDuration::from_secs(30), GraceCause::Reaped);
    let deadline = turn.grace_deadline;
    assert!(turn.latch_failure("boom".to_string()));
    assert_eq!(
        turn.grace_deadline, deadline,
        "the deadline is not restarted"
    );
    assert_eq!(turn.grace_cause, Some(GraceCause::EvidenceFailed));
    // The drain's own arm call afterwards, and the reaper's, change nothing.
    turn.arm_grace(StdDuration::from_secs(1), GraceCause::EvidenceFailed);
    turn.arm_grace(StdDuration::from_secs(1), GraceCause::Reaped);
    assert_eq!(turn.grace_deadline, deadline);
    assert_eq!(turn.grace_cause, Some(GraceCause::EvidenceFailed));
}

#[test]
fn a_latch_on_a_turn_with_no_grace_leaves_the_timer_to_the_drain() {
    let mut turn = SingleShotTurn::default();
    assert!(turn.latch_failure("boom".to_string()));
    assert!(turn.grace_deadline.is_none() && turn.grace_cause.is_none());
}

#[test]
fn a_latch_stops_the_wait_for_a_report_and_the_job_wait() {
    let mut turn = SingleShotTurn {
        waiting_for_report: true,
        background_jobs: vec![BackgroundJob {
            id: "job1".to_string(),
            kind: "local_bash".to_string(),
            description: "hung gate".to_string(),
        }],
        ..Default::default()
    };
    assert!(turn.waiting_on_jobs());
    assert!(matches!(
        turn.observe(&diagnostic("malformed CLI output: x")),
        TurnStep::EvidenceFailed { diagnostic: None }
    ));
    assert!(!turn.waiting_for_report && !turn.waiting_on_jobs());
    assert!(!turn.should_nudge());
}

// ------------------------------------------ unit: what must be recorded

/// Required status depends on the state *before* the event, not on whether
/// observing it would insert anything.
#[test]
fn only_report_evidence_and_the_completing_end_need_recording() {
    let other_call = AgentEvent::ToolCall {
        tool_use_id: CALL_B.to_string(),
        tool: "Bash".to_string(),
        input: json!({}),
    };
    let untracked_result = verdict(CALL_B, false);
    let other_result = AgentEvent::ToolResult {
        tool_use_id: CALL_A.to_string(),
        tool: "Bash".to_string(),
        output: String::new(),
        is_error: false,
    };
    let others = [
        text(),
        AgentEvent::Thinking {
            text: "hm".to_string(),
        },
        AgentEvent::SessionMeta {
            adapter_session_id: "s".to_string(),
            details: json!({}),
        },
        AgentEvent::Error {
            message: "boom".to_string(),
        },
        diagnostic("malformed CLI output: x"),
        interrupted(),
        end(true),
        AgentEvent::BackgroundJobs { running: vec![] },
        AgentEvent::SettingsNotApplied {
            verdict: crate::adapter::SettingsVerdict::Rejected,
            role: "coder".to_string(),
        },
    ];

    // Open, nothing recorded yet.
    let open = SingleShotTurn::default();
    assert!(
        open.needs_recording(&call(CALL_A, "approved")),
        "report call"
    );
    assert!(!open.needs_recording(&other_call), "non-report call");
    assert!(!open.needs_recording(&untracked_result), "untracked result");
    assert!(!open.needs_recording(&end(false)), "report-less clean end");
    for event in others.iter().chain([&other_result]) {
        assert!(!open.needs_recording(event), "{event:?}");
        assert!(
            !open.needs_recording(&wrapped(event.clone())),
            "wrapped {event:?}"
        );
    }
    assert!(!open.needs_recording(&wrapped(call(CALL_A, "approved"))));

    // Open, a report call recorded: its result (either verdict, repeated
    // or contradictory) is required; a selected report makes the clean end
    // required; an error end never is.
    let mut tracked = SingleShotTurn::default();
    feed(&mut tracked, [call(CALL_A, "approved")]);
    assert!(tracked.needs_recording(&verdict(CALL_A, false)));
    assert!(tracked.needs_recording(&verdict(CALL_A, true)));
    assert!(
        tracked.needs_recording(&other_result),
        "decided by the recorded id, as the adapter names the tool from the call"
    );
    assert!(!tracked.needs_recording(&wrapped(verdict(CALL_A, false))));
    assert!(
        !tracked.needs_recording(&end(false)),
        "nothing accepted yet"
    );
    let selected = with_accepted(CALL_A);
    assert!(selected.needs_recording(&end(false)), "the completing end");
    assert!(!selected.needs_recording(&end(true)), "an error end");
    assert!(!selected.needs_recording(&wrapped(end(false))));
    assert!(
        selected.needs_recording(&call(CALL_A, "something else")),
        "a conflicting replay"
    );
    assert!(
        selected.needs_recording(&call(CALL_A, "approved")),
        "an identical replay"
    );

    // Not open: nothing is required, whatever its shape.
    let closers: [fn(&mut SingleShotTurn); 3] = [
        |t| t.completed = true,
        |t| t.errored = true,
        |t| t.gave_up = true,
    ];
    for close in closers {
        let mut closed = with_accepted(CALL_A);
        close(&mut closed);
        for event in [
            call(CALL_C, "approved"),
            call(CALL_A, "approved"),
            verdict(CALL_A, false),
            end(false),
        ] {
            assert!(
                !closed.needs_recording(&event),
                "{event:?} on a closed turn"
            );
        }
    }
}

// ------------------------------------------------------ unit: the cutoff

#[test]
fn only_an_error_end_straight_after_a_marker_confirms_a_cutoff() {
    // Every top-level activity variant clears a pending marker.
    let activity = [
        text(),
        AgentEvent::Thinking {
            text: "hm".to_string(),
        },
        call(CALL_A, "approved"),
        verdict(CALL_A, false),
    ];
    for event in activity {
        let mut turn = SingleShotTurn::default();
        feed(&mut turn, [interrupted(), event.clone(), end(true)]);
        assert!(!turn.cutoff_pending, "{event:?}");
        assert!(!turn.cutoff_confirmed, "{event:?} clears the marker");
    }
    // Neither these nor sub-agent events touch it.
    let neutral = [
        AgentEvent::Error {
            message: "boom".to_string(),
        },
        AgentEvent::SessionMeta {
            adapter_session_id: "s".to_string(),
            details: json!({}),
        },
        AgentEvent::BackgroundJobs { running: vec![] },
        diagnostic("malformed CLI output: x"),
        AgentEvent::SettingsNotApplied {
            verdict: crate::adapter::SettingsVerdict::Rejected,
            role: "coder".to_string(),
        },
        wrapped(text()),
        wrapped(call(CALL_A, "approved")),
        wrapped(end(true)),
    ];
    for event in neutral {
        let mut turn = SingleShotTurn::default();
        feed(&mut turn, [interrupted(), event.clone()]);
        assert!(turn.cutoff_pending, "{event:?} must not clear the marker");
        feed(&mut turn, [end(true)]);
        assert!(
            turn.cutoff_confirmed,
            "{event:?} then an error end confirms"
        );
    }
    // A sub-agent's marker is not a pending cutoff.
    let mut turn = SingleShotTurn::default();
    feed(&mut turn, [wrapped(interrupted()), end(true)]);
    assert!(!turn.cutoff_pending && !turn.cutoff_confirmed);
    // A clean end clears without confirming; so does an error end with
    // nothing pending, and a later clean end does not unconfirm.
    let mut turn = SingleShotTurn::default();
    feed(&mut turn, [interrupted(), end(false)]);
    assert!(!turn.cutoff_pending && !turn.cutoff_confirmed);
    feed(&mut turn, [end(true)]);
    assert!(!turn.cutoff_confirmed);
    feed(&mut turn, [interrupted(), end(true)]);
    assert!(turn.cutoff_confirmed);
    feed(&mut turn, [text(), end(false)]);
    assert!(turn.cutoff_confirmed, "latched for the drain");
}

#[test]
fn the_cutoff_is_tracked_after_the_turn_has_closed() {
    let closers: [fn(&mut SingleShotTurn); 3] = [
        |t| t.errored = true,
        |t| t.completed = true,
        |t| t.gave_up = true,
    ];
    for close in closers {
        let mut turn = SingleShotTurn::default();
        close(&mut turn);
        feed(&mut turn, [interrupted(), end(true)]);
        assert!(
            turn.cutoff_confirmed,
            "tracked before observe's terminal guard"
        );
        feed(&mut turn, [interrupted(), text()]);
        assert!(!turn.cutoff_pending, "clearing is tracked there too");
    }
}

// -------------------------------------------------- unit: final ranking

#[test]
fn la11_a_latched_turn_ranks_below_requested_and_settings_ends_and_above_the_rest() {
    use SessionEndReason::{
        Cancelled, ClaudeSettingsRejected, ClaudeSettingsUnconfirmed, DaemonStopped, Interrupted,
        Lingered, Paused,
    };
    use SessionKind::SingleShot;
    use SessionStatus::Exited;
    let latched = |f: &dyn Fn(&mut SingleShotTurn)| {
        turn_with(|t| {
            t.failure = Some("boom".to_string());
            t.errored = true;
            f(t);
        })
    };
    let rank = |turn: &SingleShotTurn, clean, reaped, cancelled, paused, stopping| {
        final_run_state_ranked(SingleShot, turn, clean, reaped, cancelled, paused, stopping)
    };
    #[allow(clippy::type_complexity)]
    let table: Vec<(
        &str,
        SingleShotTurn,
        bool,
        bool,
        bool,
        bool,
        bool,
        Option<SessionEndReason>,
    )> = vec![
        (
            "clean exit",
            latched(&|_| {}),
            true,
            false,
            false,
            false,
            false,
            None,
        ),
        (
            "crash",
            latched(&|_| {}),
            false,
            false,
            false,
            false,
            false,
            None,
        ),
        (
            "interrupted, no confirmed cutoff",
            latched(&|t| t.interrupted = true),
            true,
            false,
            false,
            false,
            false,
            None,
        ),
        (
            "reaped, not interrupted",
            latched(&|_| {}),
            true,
            true,
            false,
            false,
            false,
            None,
        ),
        (
            "gave up",
            latched(&|t| t.gave_up = true),
            true,
            false,
            false,
            false,
            false,
            None,
        ),
        (
            "confirmed cutoff",
            latched(&|t| {
                t.interrupted = true;
                t.cutoff_confirmed = true;
            }),
            false,
            false,
            false,
            false,
            false,
            Some(Interrupted),
        ),
        (
            "confirmed cutoff, reaped and gave up",
            latched(&|t| {
                t.cutoff_confirmed = true;
                t.gave_up = true;
            }),
            true,
            true,
            false,
            false,
            false,
            Some(Interrupted),
        ),
        (
            "lingered, no cutoff",
            latched(&|t| t.lingered = true),
            false,
            false,
            false,
            false,
            false,
            Some(Lingered),
        ),
        (
            "lingered and confirmed cutoff",
            latched(&|t| {
                t.lingered = true;
                t.cutoff_confirmed = true;
            }),
            false,
            false,
            false,
            false,
            false,
            Some(Lingered),
        ),
        (
            "cancelled",
            latched(&|_| {}),
            false,
            false,
            true,
            false,
            false,
            Some(Cancelled),
        ),
        (
            "paused",
            latched(&|_| {}),
            false,
            false,
            false,
            true,
            false,
            Some(Paused),
        ),
        (
            "daemon stopped",
            latched(&|t| t.cutoff_confirmed = true),
            false,
            false,
            false,
            false,
            true,
            Some(DaemonStopped),
        ),
        (
            "cancelled beats paused and stopped",
            latched(&|t| t.lingered = true),
            false,
            false,
            true,
            true,
            true,
            Some(Cancelled),
        ),
        (
            "paused beats stopped",
            latched(&|t| t.cutoff_confirmed = true),
            false,
            false,
            false,
            true,
            true,
            Some(Paused),
        ),
    ];
    for (name, turn, clean, reaped, cancelled, paused, stopping, expected) in table {
        assert_eq!(
            rank(&turn, clean, reaped, cancelled, paused, stopping),
            (Exited, expected),
            "{name}"
        );
    }
    // Settings ends outrank the failure, requested ends outrank settings.
    for settings in [ClaudeSettingsRejected, ClaudeSettingsUnconfirmed] {
        let both = latched(&|t| {
            t.settings_end = Some(settings);
            t.lingered = true;
            t.cutoff_confirmed = true;
        });
        assert_eq!(
            rank(&both, true, false, false, false, false),
            (Exited, Some(settings))
        );
        assert_eq!(
            rank(&both, true, false, false, false, true),
            (Exited, Some(DaemonStopped)),
            "a requested end outranks settings"
        );
    }
}

#[test]
fn an_accepted_report_without_a_clean_end_is_not_no_report() {
    use SessionEndReason::Interrupted;
    use SessionStatus::Exited;
    let accepted = turn_with(|t| t.selected_report = Some(CALL_A.to_string()));
    let rank = |turn: &SingleShotTurn, clean| {
        final_run_state_ranked(
            SessionKind::SingleShot,
            turn,
            clean,
            false,
            false,
            false,
            false,
        )
    };
    assert_eq!(rank(&accepted, true), (Exited, None));
    assert_eq!(rank(&accepted, false), (Exited, None));
    let cut_off = turn_with(|t| {
        t.selected_report = Some(CALL_A.to_string());
        t.interrupted = true;
    });
    assert_eq!(rank(&cut_off, true), (Exited, Some(Interrupted)));
    // Control: nothing accepted, a clean exit, is still NoReport.
    let nothing = SingleShotTurn::default();
    assert_eq!(
        rank(&nothing, true),
        (Exited, Some(SessionEndReason::NoReport))
    );
}

#[test]
fn a_lingered_note_after_an_evidence_failure_says_what_happened() {
    let after =
        "choco stopped counting its turn because its output could not be trusted or recorded";
    let message = lingered_message(&GroupKill::Killed(1), 30.0, after);
    assert!(message.contains(after), "{message}");
    assert!(
        !message.contains("died") && !message.contains("was running"),
        "{message}"
    );
}

// ------------------------------------------------------------ drain: setup

/// Timers for scripts that must not be timed out under load.
fn la_timers(max_nudges: u32) -> TurnTimers {
    TurnTimers {
        grace: crate::test_support::LOAD_ALLOWANCE,
        nudge_after: crate::test_support::LOAD_ALLOWANCE,
        max_nudges,
        job_wait_limit: StdDuration::from_secs(60),
    }
}

/// A single-shot session on an in-memory database, with `setup` run (to
/// install triggers) before the process starts.
async fn start_prepared<F, Fut>(
    steps: Value,
    dir: &TempDir,
    timers: TurnTimers,
    setup: F,
) -> (SqlitePool, String, Arc<SessionManager>)
where
    F: FnOnce(SqlitePool, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let pool = connect_in_memory().await.unwrap();
    let session_id = seed_session(&pool).await;
    setup(pool.clone(), session_id.clone()).await;
    let adapter: Arc<dyn AgentAdapter> =
        Arc::new(ClaudeAdapter::with_binary(script_binary(&dir.0, steps)));
    let manager = SessionManager::with_turn_timers(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::new(Notify::new()),
        timers,
    );
    manager
        .start(
            &session_id,
            "claude",
            "go",
            &single_shot_role_config(),
            SessionKind::SingleShot,
        )
        .await
        .unwrap();
    (pool, session_id, manager)
}

/// Aborts every insert into `events` of this session that matches `when`
/// (a SQL predicate over `NEW`), the way a full disk or a failing trigger
/// would.
async fn abort_events(pool: &SqlitePool, session_id: &str, name: &str, when: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER {name} BEFORE INSERT ON events
         WHEN NEW.session_id = '{session_id}' AND ({when})
         BEGIN SELECT RAISE(ABORT, 'disk I/O error (test trigger)'); END"
    )))
    .execute(pool)
    .await
    .unwrap();
}

fn tool_call_with_id(id: &str) -> String {
    format!("NEW.event_type = 'tool_call' AND json_extract(NEW.payload, '$.tool_use_id') = '{id}'")
}

fn tool_result_with_id(id: &str) -> String {
    format!(
        "NEW.event_type = 'tool_result' AND json_extract(NEW.payload, '$.tool_use_id') = '{id}'"
    )
}

/// The ordinary happy script around a report: `call`, accepted, a clean end.
fn report_and_end() -> Vec<Value> {
    vec![
        raw_report_call_for(CALL_A, json!({"outcome": "done", "summary": ""})),
        raw_report_result_for(CALL_A, "done", false),
        raw_result_line(&[]),
    ]
}

fn script(mut steps: Vec<Value>) -> Value {
    steps.insert(0, json!({"op": "read_turn"}));
    Value::Array(steps)
}

/// The session's final row, waiting up to three times the usual allowance:
/// a test that expects a closed stdin to end the run must still get to its own
/// assertion when stdin is *not* closed and the 30 s grace kills the process.
async fn wait_final(pool: &SqlitePool, session_id: &str) -> chocofactory_core::models::Session {
    let allowance = crate::test_support::LOAD_ALLOWANCE * 3;
    let deadline = tokio::time::Instant::now() + allowance;
    loop {
        let run = sessions::get(pool, session_id).await.unwrap().unwrap();
        if run.status != SessionStatus::Active {
            return run;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session {session_id} still active after {allowance:?}"
        );
        tokio::time::sleep(crate::test_support::POLL_INTERVAL).await;
    }
}

async fn final_state(
    pool: &SqlitePool,
    session_id: &str,
) -> (
    chocofactory_core::models::Session,
    Vec<chocofactory_core::models::Event>,
) {
    let run = wait_final(pool, session_id).await;
    let stored = events::list_for_session(pool, session_id).await.unwrap();
    (run, stored)
}

fn count_of(stored: &[chocofactory_core::models::Event], kind: EventType) -> usize {
    stored.iter().filter(|e| e.event_type == kind).count()
}

async fn usage_rows(pool: &SqlitePool, session_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM turn_usage WHERE session_id = ?")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_exited_with_no_reason(run: &chocofactory_core::models::Session) {
    assert_eq!(
        run.status,
        SessionStatus::Exited,
        "status is not Idle: {run:?}"
    );
    assert_eq!(
        run.end_reason, None,
        "end reason None (not NoReport, and no lingered note): {run:?}"
    );
}

// ------------------------------------------------------- drain: conflicts

// LA2
#[tokio::test]
async fn la2_a_call_repeated_with_different_input_never_completes() {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_report_call_for(CALL_A, json!({"outcome": "done", "summary": ""})),
        raw_report_result_for(CALL_A, "done", false),
        raw_report_call_for(CALL_A, json!({"outcome": "done", "summary": "changed"})),
        raw_result_line(&[]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;

    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(
        protocol_errors(&stored),
        vec![format!(
            "report_outcome call '{CALL_A}' was repeated with different input; this turn cannot complete"
        )]
    );
    // The fake waits for stdin EOF and exits: nothing was left to kill.
    assert_eq!(
        session_notes(&pool, &session_id).await,
        Vec::<String>::new(),
        "no lingered note"
    );
}

/// One LA3 script: a report call, these two verdicts for it, a clean end.
async fn changed_verdict_never_completes(verdicts: [bool; 2], wording: &str) {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_report_call_for(CALL_A, json!({"outcome": "done", "summary": ""})),
        raw_report_result_for(CALL_A, "done", verdicts[0]),
        raw_report_result_for(CALL_A, "done", verdicts[1]),
        raw_result_line(&[]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;

    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(
        protocol_errors(&stored),
        vec![format!(
            "report_outcome call '{CALL_A}' {wording}; this turn cannot complete"
        )]
    );
    assert_eq!(
        session_notes(&pool, &session_id).await,
        Vec::<String>::new(),
        "no lingered note"
    );
}

// LA3, case 1
#[tokio::test]
async fn la3_case_1_an_accepted_report_later_rejected_never_completes() {
    changed_verdict_never_completes(
        [false, true],
        "got a rejected result after a successful result",
    )
    .await;
}

// LA3, case 2: not a turn that "never reported": it exited, with no reason.
#[tokio::test]
async fn la3_case_2_a_rejected_report_later_accepted_never_completes() {
    changed_verdict_never_completes(
        [true, false],
        "got a successful result after a rejected result",
    )
    .await;
}

// ---------------------------------------------- drain: ends and the exit

// LA5
#[tokio::test]
async fn la5_an_error_end_after_an_accepted_report_does_not_complete() {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_report_call(),
        raw_report_result(None),
        raw_result_line(&[("is_error", json!(true)), ("result", json!("tool crashed"))]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;
    let run = wait_final(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
}

// LA6
#[tokio::test]
async fn la6_a_report_followed_by_the_process_ending_is_not_a_complete_turn() {
    let after_report = |tail: Vec<Value>| {
        let mut steps = vec![raw_report_call(), raw_report_result(None)];
        steps.extend(tail);
        script(steps)
    };
    let cases = [
        (
            "accepted report, then a clean exit with no end",
            after_report(vec![json!({"op": "exit", "code": 0})]),
            None,
        ),
        (
            "accepted report, then a nonzero exit",
            after_report(vec![json!({"op": "exit", "code": 1})]),
            None,
        ),
        (
            "accepted report, a usage-limit marker, then a clean exit",
            after_report(vec![
                json!({"op": "raw", "line": {"type": "rate_limit_event",
                    "rate_limit_info": {"status": "rejected"}, "session_id": "s"}}),
                json!({"op": "exit", "code": 0}),
            ]),
            Some(SessionEndReason::Interrupted),
        ),
        (
            "control: no report, a report-less end, then a clean exit",
            script(vec![
                json!({"op": "text", "text": "hmm"}),
                raw_result_line(&[]),
                json!({"op": "exit", "code": 0}),
            ]),
            Some(SessionEndReason::NoReport),
        ),
    ];
    for (name, steps, reason) in cases {
        let dir = TempDir::new();
        let (pool, session_id, _manager) =
            start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;
        let run = wait_final(&pool, &session_id).await;
        assert_eq!(
            run.status,
            SessionStatus::Exited,
            "{name}: status is not Idle: {run:?}"
        );
        assert_eq!(run.end_reason, reason, "{name}: end reason");
    }
}

// LA7
#[tokio::test]
async fn la7_a_report_after_a_reportless_end_does_not_complete_by_itself() {
    let dir = TempDir::new();
    let steps = script(vec![
        json!({"op": "text", "text": "started the work in the background"}),
        raw_result_line(&[]),
        raw_report_call(),
        raw_report_result(None),
        json!({"op": "exit", "code": 0}),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;
    let run = wait_final(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    // Control: the same report followed by a clean end completes.
    let dir = TempDir::new();
    let steps = script(vec![
        json!({"op": "text", "text": "started the work in the background"}),
        raw_result_line(&[]),
        raw_report_call(),
        raw_report_result(None),
        raw_result_line(&[]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;
    let run = wait_final(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
    assert_eq!(run.end_reason, None);
}

/// A corrected report: the first rejected, the second accepted under a new
/// id, then a clean end. The completing path the rule exists for.
#[tokio::test]
async fn a_rejected_report_corrected_under_a_new_id_completes() {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_report_call_for(CALL_A, json!({"outcome": "bogus", "summary": ""})),
        raw_report_result_for(CALL_A, "bogus", true),
        raw_report_call_for(CALL_B, json!({"outcome": "done", "summary": ""})),
        raw_report_result_for(CALL_B, "done", false),
        raw_result_line(&[]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |_, _| async {}).await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
    assert_eq!(run.end_reason, None);
    assert_eq!(protocol_errors(&stored), Vec::<String>::new());
    assert_eq!(count_of(&stored, EventType::TurnCompleted), 1);
    assert_eq!(usage_rows(&pool, &session_id).await, 1);
}

// --------------------------------------------- drain: unrecordable rows

/// One LA8 case: the row `when` selects is lost; the turn needs it.
async fn a_needed_row_is_lost(name: &str, when: String, event_type: &str) {
    let dir = TempDir::new();
    let (log, _guard) = capture_logs();
    let (pool, session_id, _manager) = start_prepared(
        script(report_and_end()),
        &dir,
        la_timers(0),
        |pool, sid| async move {
            abort_events(&pool, &sid, "abort_needed", &when).await;
        },
    )
    .await;

    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    let errors = protocol_errors(&stored);
    assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    assert!(
        errors[0].starts_with(&format!("choco could not record a {event_type} event (")),
        "{name}: {}",
        errors[0]
    );
    assert!(
        errors[0].ends_with("); this turn cannot complete"),
        "{name}: {}",
        errors[0]
    );
    assert_eq!(
        stored
            .iter()
            .filter(|e| e.event_type.to_string() == event_type)
            .count(),
        0,
        "{name}: the failed row is missing"
    );
    if event_type == "turn_completed" {
        assert_eq!(
            usage_rows(&pool, &session_id).await,
            0,
            "{name}: no usage row"
        );
    }
    assert_eq!(
        session_notes(&pool, &session_id).await,
        Vec::<String>::new(),
        "{name}: no lingered note"
    );
    assert!(
        log.text()
            .lines()
            .any(|l| l.contains("failed to append event") && l.contains(event_type)),
        "{name}: {}",
        log.text()
    );
}

// LA8, the report call
#[tokio::test]
async fn la8_a_report_call_that_could_not_be_recorded_never_completes() {
    a_needed_row_is_lost("report call", tool_call_with_id(CALL_A), "tool_call").await;
}

// LA8, a tracked report result
#[tokio::test]
async fn la8_a_report_result_that_could_not_be_recorded_never_completes() {
    a_needed_row_is_lost("report result", tool_result_with_id(CALL_A), "tool_result").await;
}

// LA8, the completing end
#[tokio::test]
async fn la8_a_completing_end_that_could_not_be_recorded_never_completes() {
    a_needed_row_is_lost(
        "completing end",
        "NEW.event_type = 'turn_completed'".to_string(),
        "turn_completed",
    )
    .await;
}

/// The failure of the usage row rolls its event back, and a failed event
/// leaves no usage row: both directions of the end's atomicity, together
/// with `db::usage`'s own test.
#[tokio::test]
async fn a_failing_usage_insert_loses_the_completing_end_and_latches() {
    let dir = TempDir::new();
    let (pool, session_id, _manager) = start_prepared(
        script(report_and_end()),
        &dir,
        la_timers(0),
        |pool, sid| async move {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TRIGGER abort_usage BEFORE INSERT ON turn_usage
                 WHEN NEW.session_id = '{sid}'
                 BEGIN SELECT RAISE(ABORT, 'disk I/O error (test trigger)'); END"
            )))
            .execute(&pool)
            .await
            .unwrap();
        },
    )
    .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(
        count_of(&stored, EventType::TurnCompleted),
        0,
        "the event rolled back"
    );
    assert_eq!(usage_rows(&pool, &session_id).await, 0);
    let errors = protocol_errors(&stored);
    assert!(
        errors.len() == 1
            && errors[0].starts_with("choco could not record a turn_completed event ("),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_healthy_completion_writes_exactly_one_end_and_its_usage_row() {
    let dir = TempDir::new();
    let (pool, session_id, _manager) =
        start_prepared(script(report_and_end()), &dir, la_timers(0), |_, _| async {
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
    assert_eq!(count_of(&stored, EventType::TurnCompleted), 1);
    assert_eq!(usage_rows(&pool, &session_id).await, 1);
}

/// The other half of "only evidence": a row that is not needed, lost, does
/// not fail the turn.
#[tokio::test]
async fn la8_control_1_a_lost_assistant_message_does_not_fail_the_turn() {
    let dir = TempDir::new();
    let (log, _guard) = capture_logs();
    let mut steps = vec![json!({"op": "text", "text": "boom"})];
    steps.extend(report_and_end());
    let (pool, session_id, _manager) =
        start_prepared(script(steps), &dir, la_timers(0), |pool, sid| async move {
            abort_events(
                &pool,
                &sid,
                "abort_chatter",
                "NEW.event_type = 'assistant_message' AND json_extract(NEW.payload, '$.text') = 'boom'",
            )
            .await;
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
    assert_eq!(run.end_reason, None);
    assert_eq!(
        count_of(&stored, EventType::AssistantMessage),
        0,
        "the row is missing"
    );
    assert_eq!(protocol_errors(&stored), Vec::<String>::new());
    assert!(
        log.text()
            .lines()
            .any(|l| l.contains("failed to append event") && l.contains("assistant_message")),
        "{}",
        log.text()
    );
}

#[tokio::test]
async fn la8_control_2_a_late_lost_report_call_after_completion_is_history() {
    let dir = TempDir::new();
    let mut steps = report_and_end();
    steps.push(raw_report_call_for(
        CALL_C,
        json!({"outcome": "done", "summary": ""}),
    ));
    steps.push(raw_report_result_for(CALL_C, "done", false));
    let (pool, session_id, _manager) =
        start_prepared(script(steps), &dir, la_timers(0), |pool, sid| async move {
            abort_events(&pool, &sid, "abort_late", &tool_call_with_id(CALL_C)).await;
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
    assert_eq!(run.end_reason, None);
    assert_eq!(
        protocol_errors(&stored),
        Vec::<String>::new(),
        "no synthetic error for C"
    );
    assert!(
        !stored
            .iter()
            .any(|e| e.payload["tool_use_id"] == CALL_C && e.event_type == EventType::ToolCall),
        "C's call row is missing"
    );
}

#[tokio::test]
async fn la8_control_3_a_lost_reportless_end_does_not_fail_the_turn() {
    let dir = TempDir::new();
    let (log, _guard) = capture_logs();
    let mut steps = vec![
        json!({"op": "text", "text": "started"}),
        raw_result_line(&[]),
    ];
    steps.extend(report_and_end());
    let (pool, session_id, _manager) =
        start_prepared(script(steps), &dir, la_timers(0), |pool, sid| async move {
            // Only the end that comes before any report: it cannot also
            // reject the completing end that follows.
            abort_events(
                &pool,
                &sid,
                "abort_first_end",
                "NEW.event_type = 'turn_completed' AND NOT EXISTS (
                    SELECT 1 FROM events e WHERE e.session_id = NEW.session_id
                    AND e.event_type = 'tool_result')",
            )
            .await;
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "{run:?}");
    assert_eq!(run.end_reason, None);
    assert_eq!(
        count_of(&stored, EventType::TurnCompleted),
        1,
        "only the completing end"
    );
    assert_eq!(usage_rows(&pool, &session_id).await, 1);
    assert_eq!(protocol_errors(&stored), Vec::<String>::new());
    assert!(
        log.text()
            .lines()
            .any(|l| l.contains("failed to append event") && l.contains("turn_completed")),
        "the first append failure was logged: {}",
        log.text()
    );
}

/// An error end whose row is lost still ends the turn in memory.
#[tokio::test]
async fn a_lost_error_end_still_ends_the_turn() {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_report_call(),
        raw_report_result(None),
        raw_result_line(&[("is_error", json!(true)), ("result", json!("tool crashed"))]),
        // If the lost error end had not ended the turn, this would complete it.
        raw_result_line(&[]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |pool, sid| async move {
            abort_events(
                &pool,
                &sid,
                "abort_error_end",
                "NEW.event_type = 'turn_completed' AND json_extract(NEW.payload, '$.is_error') = 1",
            )
            .await;
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(
        protocol_errors(&stored),
        Vec::<String>::new(),
        "no latch, no diagnostic"
    );
}

/// A recording failure is the turn's first failure: a conflicting replay
/// whose own row is lost fails as unrecordable, with no second reason.
#[tokio::test]
async fn a_conflicting_replay_that_could_not_be_recorded_fails_as_unrecorded() {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_report_call_for(CALL_A, json!({"outcome": "done", "summary": ""})),
        raw_report_result_for(CALL_A, "done", false),
        raw_report_call_for(CALL_A, json!({"outcome": "done", "summary": "changed"})),
        raw_result_line(&[]),
    ]);
    let (pool, session_id, _manager) =
        start_prepared(steps, &dir, la_timers(0), |pool, sid| async move {
            abort_events(
                &pool,
                &sid,
                "abort_conflict",
                "NEW.event_type = 'tool_call' AND json_extract(NEW.payload, '$.input.summary') = 'changed'",
            )
            .await;
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    let errors = protocol_errors(&stored);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("choco could not record a tool_call event ("),
        "{errors:?}"
    );
    assert!(
        !errors[0].contains("repeated with different input"),
        "{errors:?}"
    );
}

/// The diagnostic's own write failing leaves the latch in place, logs at
/// error level, and does not recurse.
#[tokio::test]
async fn a_failed_diagnostic_write_keeps_the_latch() {
    let dir = TempDir::new();
    let (log, _guard) = capture_logs();
    let (pool, session_id, _manager) = start_prepared(
        script(report_and_end()),
        &dir,
        la_timers(0),
        |pool, sid| async move {
            abort_events(&pool, &sid, "abort_call", &tool_call_with_id(CALL_A)).await;
            abort_events(
                &pool,
                &sid,
                "abort_diagnostic",
                "NEW.event_type = 'error' AND json_extract(NEW.payload, '$.protocol_error') = 1",
            )
            .await;
        },
    )
    .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(
        protocol_errors(&stored),
        Vec::<String>::new(),
        "no misleading row"
    );
    let diagnostic_failures = log
        .text()
        .lines()
        .filter(|l| {
            l.contains("ERROR") && l.contains("failed to append event") && l.contains("diagnostic")
        })
        .count();
    assert_eq!(diagnostic_failures, 1, "{}", log.text());
}

/// An incoming `ProtocolError` whose own ordinary write fails still fails
/// the turn, and adds no diagnostic of its own.
#[tokio::test]
async fn an_incoming_diagnostic_whose_write_fails_still_fails_the_turn() {
    let dir = TempDir::new();
    let mut steps = vec![json!({"op": "raw", "line": {
        "type": "assistant",
        "message": {"content": [{"type": "tool_use", "name": tool()}]},
        "session_id": "s"}})];
    steps.extend(report_and_end());
    let (pool, session_id, _manager) =
        start_prepared(script(steps), &dir, la_timers(0), |pool, sid| async move {
            abort_events(
                &pool,
                &sid,
                "abort_diagnostic",
                "NEW.event_type = 'error' AND json_extract(NEW.payload, '$.protocol_error') = 1",
            )
            .await;
        })
        .await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(protocol_errors(&stored), Vec::<String>::new());
}

// ------------------------------------------------------ drain: diagnostics

// LA9
#[tokio::test]
async fn la9_a_main_agents_diagnostic_fails_the_turn_a_helpers_does_not() {
    let malformed = |parent: Option<&str>| {
        let mut line = json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "name": tool(),
                                      "input": {"outcome": "done"}}]},
            "session_id": "s",
        });
        if let Some(parent) = parent {
            line["parent_tool_use_id"] = json!(parent);
        }
        json!({"op": "raw", "line": line})
    };

    // Top level: a valid report and clean end afterwards cannot complete it.
    let dir = TempDir::new();
    let mut steps = vec![malformed(None)];
    steps.extend(report_and_end());
    let (pool, session_id, _manager) =
        start_prepared(script(steps), &dir, la_timers(0), |_, _| async {}).await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_exited_with_no_reason(&run);
    assert_eq!(
        protocol_errors(&stored),
        vec!["malformed CLI output: a tool_use block's id is missing"]
    );
    assert!(
        stored
            .iter()
            .filter(|e| e.payload["protocol_error"] == true)
            .all(|e| e.payload.get("parent_tool_use_id").is_none()),
        "top-level diagnostic carries no helper scope"
    );

    // The same block under a readable helper parent: scoped to the helper.
    let dir = TempDir::new();
    let mut steps = vec![malformed(Some(HELPER))];
    steps.extend(report_and_end());
    let (pool, session_id, _manager) =
        start_prepared(script(steps), &dir, la_timers(0), |_, _| async {}).await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Idle, "sub-agent half: {run:?}");
    assert_eq!(run.end_reason, None);
    let scoped: Vec<_> = stored
        .iter()
        .filter(|e| e.payload["protocol_error"] == true)
        .collect();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].payload["parent_tool_use_id"], HELPER);
}

// ------------------------------------------------------- drain: standing

// LA10
#[tokio::test]
async fn la10_a_standing_session_records_a_diagnostic_and_a_lost_row_and_carries_on() {
    let dir = TempDir::new();
    let (log, _guard) = capture_logs();
    let steps = script(vec![
        raw_result_line(&[("parent_tool_use_id", json!(7))]),
        json!({"op": "text", "text": "dropped"}),
        json!({"op": "read_turn"}),
        json!({"op": "text", "text": "second reply"}),
    ]);
    let pool = connect_in_memory().await.unwrap();
    let session_id = seed_session(&pool).await;
    abort_events(
        &pool,
        &session_id,
        "abort_dropped",
        "NEW.event_type = 'assistant_message' AND json_extract(NEW.payload, '$.text') = 'dropped'",
    )
    .await;
    let adapter: Arc<dyn AgentAdapter> =
        Arc::new(ClaudeAdapter::with_binary(script_binary(&dir.0, steps)));
    let manager = SessionManager::with_turn_timers(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::new(Notify::new()),
        la_timers(0),
    );
    let cfg = role_config();
    manager
        .start(&session_id, "claude", "go", &cfg, SessionKind::Standing)
        .await
        .unwrap();
    crate::test_support::wait_until("the incoming diagnostic to be recorded", || async {
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        if protocol_errors(&stored).len() == 1 {
            Ok(())
        } else {
            Err(format!("{stored:?}"))
        }
    })
    .await;

    manager
        .send_message(&session_id, "again", &cfg)
        .await
        .unwrap();
    // Either the reply arrives, or the session ended under us.
    crate::test_support::wait_until("the second reply, or the session's end", || async {
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        if stored.iter().any(|e| e.payload["text"] == "second reply")
            || run.status != SessionStatus::Active
        {
            Ok(())
        } else {
            Err(format!("{stored:?}"))
        }
    })
    .await;
    let stored = events::list_for_session(&pool, &session_id).await.unwrap();
    assert!(
        stored.iter().any(|e| e.payload["text"] == "second reply"),
        "stdin still open: the session answered a second turn: {stored:?}"
    );
    let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
    assert_eq!(run.status, SessionStatus::Active, "{run:?}");
    assert_eq!(
        protocol_errors(&stored),
        vec![
            "malformed CLI output: parent_tool_use_id is a number, not a non-empty string or null"
        ],
        "only the incoming diagnostic: no latch-driven one"
    );
    assert!(
        !stored.iter().any(|e| e.payload["text"] == "dropped"),
        "the lost row is missing"
    );
    assert!(
        log.text()
            .lines()
            .any(|l| l.contains("failed to append event") && l.contains("assistant_message")),
        "{}",
        log.text()
    );

    manager.cancel(&session_id).await.unwrap();
    let run = wait_final(&pool, &session_id).await;
    assert_eq!(run.end_reason, Some(SessionEndReason::Cancelled), "{run:?}");
}

// ------------------------------------------------- drain: lingering latch

/// A latched turn whose process ignores the closed stdin is killed once its
/// grace runs out, and the note says why in the evidence-failure words.
#[tokio::test]
async fn a_latched_turn_that_keeps_running_ends_lingered_with_the_evidence_failure_phrase() {
    let dir = TempDir::new();
    let steps = script(vec![
        raw_result_line(&[("parent_tool_use_id", json!(7))]),
        json!({"op": "emit_forever", "text": "still writing files"}),
    ]);
    let timers = TurnTimers {
        grace: StdDuration::from_millis(400),
        ..la_timers(0)
    };
    let (pool, session_id, _manager) = start_prepared(steps, &dir, timers, |_, _| async {}).await;
    let (run, stored) = final_state(&pool, &session_id).await;
    assert_eq!(run.status, SessionStatus::Exited, "{run:?}");
    assert_eq!(run.end_reason, Some(SessionEndReason::Lingered));
    let lingered: Vec<_> = stored
        .iter()
        .filter(|e| e.event_type == EventType::SessionNote && e.payload["kind"] == "lingered")
        .map(|e| e.payload["message"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(lingered.len(), 1, "{lingered:?}");
    assert!(
        lingered[0].contains(
            "choco stopped counting its turn because its output could not be trusted or recorded"
        ),
        "{lingered:?}"
    );
}

// ----------------------------------------------------- drain: cutoff cases

// LA12
#[tokio::test]
async fn la12_a_latched_turn_is_interrupted_only_if_the_limit_actually_ended_it() {
    let structured_assistant = json!({"op": "raw", "line": {
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "You've hit your limit"}]},
        "error": "rate_limit", "isApiErrorMessage": true, "apiErrorStatus": 429,
        "session_id": "s"}});
    let rejected_limit = |parent: Option<&str>| {
        let mut line = json!({"type": "rate_limit_event",
            "rate_limit_info": {"status": "rejected"}, "session_id": "s"});
        if let Some(parent) = parent {
            line["parent_tool_use_id"] = json!(parent);
        }
        json!({"op": "raw", "line": line})
    };
    let failed_end = raw_result_line(&[
        ("is_error", json!(true)),
        ("result", json!("tool execution failed")),
    ]);
    let cases = [
        (
            "main usage_limit op",
            vec![json!({"op": "usage_limit"})],
            Some(SessionEndReason::Interrupted),
        ),
        (
            "structured marker, then an Error and an error end",
            vec![structured_assistant, failed_end.clone()],
            Some(SessionEndReason::Interrupted),
        ),
        (
            "control A: an earlier marker, then activity, then a non-limit error end",
            vec![
                rejected_limit(None),
                json!({"op": "text", "text": "carrying on"}),
                failed_end.clone(),
            ],
            None,
        ),
        (
            "control B: a helper's marker, then a non-limit error end",
            vec![rejected_limit(Some(HELPER)), failed_end],
            None,
        ),
    ];
    for (name, tail, reason) in cases {
        let dir = TempDir::new();
        let mut steps = vec![raw_report_call()];
        steps.extend(tail);
        let (pool, session_id, _manager) =
            start_prepared(script(steps), &dir, la_timers(0), |pool, sid| async move {
                abort_events(&pool, &sid, "abort_call", &tool_call_with_id(CALL_A)).await;
            })
            .await;
        let run = wait_final(&pool, &session_id).await;
        assert_eq!(run.status, SessionStatus::Exited, "{name}: status: {run:?}");
        assert_eq!(run.end_reason, reason, "{name}: end reason");
    }
}

// ------------------------------------------------ drain: per-drain state

/// Rows from an earlier drain are history: a new drain over a session with
/// an accepted report already on its timeline still needs its own.
#[tokio::test]
async fn a_new_drain_does_not_inherit_a_report_from_earlier_rows() {
    for resumed in [false, true] {
        let dir = TempDir::new();
        let pool = connect_in_memory().await.unwrap();
        let session_id = seed_session(&pool).await;
        for (kind, payload) in [
            (
                EventType::ToolCall,
                json!({"tool_use_id": CALL_C, "tool": tool(), "input": {"outcome": "done"}}),
            ),
            (
                EventType::ToolResult,
                json!({"tool_use_id": CALL_C, "tool": tool(), "output": "ok", "is_error": false}),
            ),
            (EventType::TurnCompleted, json!({"is_error": false})),
        ] {
            events::append(&pool, &session_id, kind, payload)
                .await
                .unwrap();
        }
        let steps = script(vec![
            json!({"op": "text", "text": "hello again"}),
            raw_result_line(&[]),
            json!({"op": "exit", "code": 0}),
        ]);
        let adapter: Arc<dyn AgentAdapter> =
            Arc::new(ClaudeAdapter::with_binary(script_binary(&dir.0, steps)));
        let manager = SessionManager::with_turn_timers(
            pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::new(Notify::new()),
            la_timers(0),
        );
        let cfg = single_shot_role_config();
        if resumed {
            manager
                .resume(
                    &session_id,
                    "claude",
                    "11111111-2222-4333-8444-555555555555",
                    "go on",
                    &cfg,
                    SessionKind::SingleShot,
                )
                .await
                .unwrap();
        } else {
            manager
                .start(&session_id, "claude", "go", &cfg, SessionKind::SingleShot)
                .await
                .unwrap();
        }
        let run = wait_final(&pool, &session_id).await;
        assert_eq!(
            run.status,
            SessionStatus::Exited,
            "resumed={resumed}: {run:?}"
        );
        assert_eq!(
            run.end_reason,
            Some(SessionEndReason::NoReport),
            "resumed={resumed}: rows from before are not this drain's evidence"
        );
    }
}

// ------------------------------------------------- the database's own wait

fn parse_elapsed_ms(line: &str) -> u64 {
    let (_, rest) = line.split_once("append_elapsed_ms=").expect(line);
    rest.split_whitespace().next().unwrap().parse().expect(line)
}

fn append_elapsed(log: &str, needle: &str, event_type: &str) -> u64 {
    let line = log
        .lines()
        .find(|l| l.contains(needle) && l.contains(&format!("event_type={event_type}")))
        .unwrap_or_else(|| panic!("no {needle:?} line for {event_type} in: {log}"));
    parse_elapsed_ms(line)
}

/// A file-backed database under the real `$TMPDIR` whose write lock a
/// separate connection holds while the turn's report call is appended.
async fn run_with_a_held_lock(
    hold: StdDuration,
) -> (
    chocofactory_core::models::Session,
    Vec<chocofactory_core::models::Event>,
    String,
) {
    use sqlx::Connection;
    use std::str::FromStr;

    let dir = TempDir::new();
    let (log, _guard) = capture_logs();
    let db_path = dir.0.join("choco.db");
    let pool = crate::db::pool::connect(&db_path).await.unwrap();

    let release = dir.0.join("release");
    let session_id = seed_session(&pool).await;
    let mut steps = vec![json!({"op": "wait_file", "path": release})];
    steps.extend(report_and_end());
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(script_binary(
        &dir.0,
        script(steps),
    )));
    let manager = SessionManager::with_turn_timers(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::new(Notify::new()),
        la_timers(0),
    );
    manager
        .start(
            &session_id,
            "claude",
            "go",
            &single_shot_role_config(),
            SessionKind::SingleShot,
        )
        .await
        .unwrap();
    // The init row (and the adapter session id) are persisted before the
    // lock is taken, so they cannot spend the wait meant for the report.
    crate::test_support::wait_until("the init row to be persisted", || async {
        let stored = events::list_for_session(&pool, &session_id).await.unwrap();
        if stored
            .iter()
            .any(|e| e.event_type == EventType::SessionMeta)
        {
            Ok(())
        } else {
            Err(format!("{stored:?}"))
        }
    })
    .await;

    let options =
        sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .unwrap();
    let mut holder = sqlx::SqliteConnection::connect_with(&options)
        .await
        .unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut holder)
        .await
        .unwrap();
    std::fs::write(&release, "go").unwrap();
    tokio::time::sleep(hold).await;
    sqlx::query("COMMIT").execute(&mut holder).await.unwrap();
    drop(holder);

    // Bounded, and returning the row whatever its status: a pool whose
    // writes fail fast can lose even the final status write, and the test
    // should then fail on the status, not wait it out.
    let deadline = tokio::time::Instant::now() + crate::test_support::LOAD_ALLOWANCE;
    let run = loop {
        let run = sessions::get(&pool, &session_id).await.unwrap().unwrap();
        if run.status != SessionStatus::Active || tokio::time::Instant::now() >= deadline {
            break run;
        }
        tokio::time::sleep(crate::test_support::POLL_INTERVAL).await;
    };
    let stored = events::list_for_session(&pool, &session_id).await.unwrap();
    let text = log.text();
    drop(manager);
    (run, stored, text)
}

// LA13: the pool keeps SQLx's default wait, and the tests below measure it.
#[tokio::test]
async fn la13_the_production_pool_keeps_the_five_second_busy_wait() {
    let dir = TempDir::new();
    let pool = crate::db::pool::connect(&dir.0.join("choco.db"))
        .await
        .unwrap();
    let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(busy, 5000, "SQLx's default busy wait, kept");
}

// LA13, healthy half: a lock released inside SQLx's wait costs time only.
#[tokio::test]
async fn la13_a_lock_released_within_the_busy_wait_does_not_fail_the_turn() {
    let (run, stored, log) = run_with_a_held_lock(StdDuration::from_secs(1)).await;
    assert_eq!(
        run.status,
        SessionStatus::Idle,
        "status is not Idle: {run:?}"
    );
    assert_eq!(run.end_reason, None);
    assert_eq!(protocol_errors(&stored), Vec::<String>::new());
    let waited = append_elapsed(&log, "appended event", "tool_call");
    eprintln!("LA13 measured: the report call append waited {waited} ms for a 1 s lock");
    assert!(
        (500..5000).contains(&waited),
        "the append waited out the lock, inside the 5 s wait: {waited} ms"
    );
}

// LA13, failing half: a lock that outlasts it fails the append at about 5 s.
#[tokio::test]
async fn la13_a_lock_held_past_the_busy_wait_fails_the_append_at_about_five_seconds() {
    let (run, stored, log) = run_with_a_held_lock(StdDuration::from_secs(7)).await;
    assert_exited_with_no_reason(&run);
    assert!(
        !stored.iter().any(|e| e.event_type == EventType::ToolCall),
        "no row for the call: {stored:?}"
    );
    let errors = protocol_errors(&stored);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("choco could not record a tool_call event ("),
        "{errors:?}"
    );
    assert!(errors[0].contains("database is locked"), "{errors:?}");
    let waited = append_elapsed(&log, "failed to append event", "tool_call");
    eprintln!(
        "LA13 measured: the failed report call append gave up after {waited} ms (lock held 7 s)"
    );
    assert!(
        waited >= 4500,
        "the append waited out the busy timeout: {waited} ms"
    );
    assert!(
        waited <= 5000 + crate::test_support::LOAD_ALLOWANCE.as_millis() as u64,
        "the append gave up with the wait, not with the lock: {waited} ms"
    );
}
