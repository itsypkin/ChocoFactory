use super::stage_capture::{
    MAX_CAPTURE_BYTES, capture_label, derive_agent_reply_capture, outcome_from_report,
    turn_outcome, unwrap_code_fence,
};
use super::sweep::agent_reason;
use super::*;

/// How often the `agent_turn` completion watcher polls a `session`'s
/// status. Not configurable (yet) — this is an internal implementation
/// detail of auto-advancing single-shot turns, not a user-facing knob.
const TURN_WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// What a completed `agent_turn` transitions on when its reply carried no
/// verdict of its own — §5.2's "a plain single-shot turn just emits `done`".
pub(super) const TURN_DEFAULT_OUTCOME: &str = "done";

/// The note a restart sweep adds when it cannot compare a stranded turn's
/// worktree with its baseline. Fails closed like `read_only_verdict`: only a
/// role that resolves to not-read-only gets no note.
pub(super) fn unverified_note(
    definition: &WorkflowDefinition,
    stage: &str,
    error: &dyn std::fmt::Display,
) -> Option<String> {
    let role = match definition.stages.get(stage).map(|s| &s.kind) {
        Some(StageKind::AgentTurn { role, .. }) => Some(role.as_str()),
        _ => None,
    };
    let resolved = role.and_then(|r| definition.roles.get(r));
    if resolved.is_some_and(|r| !r.read_only) {
        return None;
    }
    Some(match role {
        Some(role) => format!(
            "could not verify that read-only role '{role}' left the worktree unchanged in \
             stage '{stage}': {error}; inspect and reset the worktree before retrying"
        ),
        None => format!(
            "could not verify that stage '{stage}' left the worktree unchanged: it is not an \
             agent_turn stage in the workflow definition ({error}); inspect and reset the \
             worktree before retrying"
        ),
    })
}

/// What the post-turn check of a read-only role found (#172).
pub(super) enum ReadOnlyVerdict {
    /// Not a read-only role, or the worktree matches its baseline.
    Clean,
    /// The worktree changed; carries the stuck reason.
    Violation(String),
    /// The check could not run; carries the stuck reason.
    Unverified(String),
}

/// How many times in a row one interrupted session may be picked up again
/// before a retry insists on a fresh start (#92).
///
/// A resumed turn that is interrupted again is resumable again — which is
/// right for a usage limit that has since reset, and wrong for anything
/// that keeps interrupting a session the moment it wakes. Three attempts is
/// enough for the first and short enough that the second is noticed. The
/// chain resets whenever a stage starts a fresh session.
pub(super) const MAX_CONSECUTIVE_RESUMES: usize = 3;

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn branch_label(branch: &str) -> &str {
    if branch.is_empty() {
        "(detached)"
    } else {
        branch
    }
}

/// What the daemon says to a turn it has just resumed (#92).
///
/// Deliberately short, and deliberately not the stage's prompt: the agent
/// still has that, and everything it did before the interruption, in the
/// session being resumed. What it cannot know is that it was interrupted at
/// all — from inside the transcript, the limit message is simply the last
/// thing that happened — so this says what stopped it, that its work is
/// still on disk, and that the turn still ends the way #90 requires.
fn resume_prompt(resume: &ResumeSession) -> String {
    format!(
        "Your previous turn on this stage was interrupted before you could finish: {}. \
         Nothing you did was rolled back — your working tree still holds it. Check `git status` \
         and `git diff` to see where you got to, continue from there rather than starting over, \
         and finish the stage by calling `report_outcome` as instructed.",
        resume.describe()
    )
}

/// The interrupted agent session a re-entered stage should continue rather
/// than replace (#92).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResumeSession {
    /// The CLI's own session id to resume, as recorded by the previous
    /// session.
    pub(super) adapter_session_id: String,
    /// That session, so the new one can point back at it (`resumed_from`)
    /// and the timeline can name it.
    pub(super) previous_session_id: String,
    /// Why that session's turn ended — the thing that made it resumable,
    /// and what the resumed turn is told about its own interruption.
    pub(super) end_reason: SessionEndReason,
}

impl ResumeSession {
    /// How the interruption is described to the agent being resumed and to
    /// whoever reads the timeline.
    fn describe(&self) -> &'static str {
        match self.end_reason {
            SessionEndReason::Interrupted => "your account hit a usage limit",
            SessionEndReason::DaemonStopped => {
                "the daemon was stopped or restarted while you were working"
            }
            SessionEndReason::Reaped => {
                "the daemon closed it after it went quiet for longer than its idle timeout"
            }
            // Unreachable: `resumable_session` admits no other reason. A
            // plain sentence rather than an `unreachable!()`, because this
            // only feeds a prompt and a note — nothing here is worth
            // panicking a live daemon over.
            _ => "it was interrupted",
        }
    }
}

impl WorkflowEngine {
    pub(super) async fn enter_agent_turn(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name,
            stage_def,
            payload,
            input,
            resume,
        } = *entry;
        let StageKind::AgentTurn {
            role,
            prompt_file,
            capture,
            report_sections,
        } = &stage_def.kind
        else {
            unreachable!("enter_agent_turn is only called for AgentTurn stages")
        };
        let prompt_file = prompt_file.as_deref();
        let capture = *capture;
        // `WorkflowDefinition::parse`/`load` reject an agent_turn stage
        // with an unknown role, but `roles`/`stages` are `pub` fields with
        // no private-construction guard — a definition built by hand
        // (struct literal) rather than through those constructors could
        // reach here unvalidated, so this stays a reported error rather
        // than an `.expect()` (§ review on PR #35).
        let role_def = definition
            .roles
            .get(role)
            .ok_or_else(|| EngineError::UnknownRole {
                stage: stage_name.to_string(),
                role: role.to_string(),
            })?;

        // A resumed turn is already holding the stage's prompt: it read it,
        // worked on it, and was cut off mid-way (#92). Sending the same
        // prompt again would read as a second, identical assignment, so it
        // is told what happened to it instead. Everything else about the
        // turn — role, isolation, the `report_outcome` instruction the
        // adapter appends — resolves exactly as for a fresh one, so #90's
        // completion contract is unchanged.
        let prompt = match (resume, prompt_file) {
            (Some(resume), _) => resume_prompt(resume),
            // A workflow-authored prompt is templated against earlier stages'
            // captures (P2-3, §5.1) — this is how a reviewer's verdict
            // reaches the coder's next turn. Live human input is not: it's
            // what a person typed, and quietly rewriting parts of it would be
            // both surprising and a way to smuggle payload contents into a
            // message the human believes they authored.
            (None, Some(path)) => {
                let raw = fs::read_to_string(path).map_err(EngineError::Io)?;
                let (rendered, unresolved) =
                    template::render(&raw, payload).map_err(|err| EngineError::Template {
                        stage: stage_name.to_string(),
                        reason: err.to_string(),
                    })?;
                self.record_unresolved_template_note(task_id, stage_name, &unresolved)
                    .await;
                rendered
            }
            (None, None) => input
                .ok_or_else(|| EngineError::MissingAgentTurnInput(stage_name.to_string()))?
                .to_string(),
        };

        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;
        let global = self
            .load_global_config()
            .map_err(EngineError::GlobalConfig)?;
        let cwd = working_dir(&task, definition)?;
        // Issue #73: the stage's own `on:` edge names, so `report_outcome`'s
        // allowed values can never disagree with what this stage can
        // actually route on. `IndexMap::keys()` preserves declaration order,
        // which only matters for how the tool's schema/description read —
        // routing itself doesn't care about order.
        //
        // Gated on `capture: json`, the same marker that means "this stage
        // routes on the agent's own verdict" (see `finish_turn`): only then do
        // the `on:` keys become reportable outcomes. Without this gate every
        // ordinary `on:` edge (present on nearly every agent_turn) would turn
        // into a verdict the stage can't actually honor.
        //
        // #90: every *other* stage that can conclude on its own gets `done`
        // (`coding`/`revising` declare `on: {done: ...}` with no `capture:`).
        // A single-shot turn now completes only once it reports (see
        // `session::drain_session`), and `done` is the one outcome such a
        // stage ever advances on, so it's also the only one the tool should
        // accept. A standing stage (empty `on:`, chat) never concludes, so it
        // gets nothing and no instruction to report.
        let report_outcomes: Vec<String> = if capture == Some(Capture::Json) {
            stage_def.on.keys().cloned().collect()
        } else if stage_def.on.is_empty() {
            Vec::new()
        } else {
            vec![TURN_DEFAULT_OUTCOME.to_string()]
        };
        let resolved = role_config::resolve(
            role,
            role_def,
            &global,
            &task.config,
            cwd,
            definition.worktree,
            role_config::StageReport {
                outcomes: report_outcomes,
                // #95: straight from the stage definition, like the
                // outcomes above. Enforcement lives in the tool the turn
                // calls, so nothing downstream of here — routing,
                // `finish_turn`, the capture — changes shape.
                sections: report_sections.to_vec(),
            },
        )
        .map_err(EngineError::RoleConfig)?;

        // #172: a read-only role's baseline is taken (or, for a resumed turn,
        // looked up) before any session row exists, so a failure here leaves
        // nothing behind and the agent is never started.
        let baseline = if role_def.read_only {
            Some(
                self.read_only_baseline(
                    stage_name,
                    role,
                    &resolved.role_config.cwd,
                    resume.map(|r| r.previous_session_id.as_str()),
                )
                .await?,
            )
        } else {
            None
        };

        let new_session = sessions::NewSession {
            task_id,
            stage: stage_name,
            role,
            cli_adapter: &resolved.cli,
            model: &resolved.model,
        };
        // A resume still opens its own session (#92): the attempt history
        // stays one row per attempt, and `resumed_from` is what records
        // that this attempt continued the previous one's conversation
        // rather than starting another.
        let session = match resume {
            Some(resume) => {
                sessions::create_resumed(
                    &self.pool,
                    new_session,
                    sessions::ResumedFrom {
                        session_id: &resume.previous_session_id,
                        adapter_session_id: &resume.adapter_session_id,
                    },
                )
                .await?
            }
            None => sessions::create(&self.pool, new_session).await?,
        };

        // Written before the agent is spawned, so it always precedes anything
        // the turn does.
        if let Some(baseline) = baseline
            && let Err(err) = events::append(
                &self.pool,
                &session.id,
                EventType::WorktreeBaseline,
                baseline,
            )
            .await
        {
            tracing::error!(session_id = %session.id, %err, "failed to record the worktree baseline");
            if let Err(update_err) = sessions::update_status(
                &self.pool,
                &session.id,
                SessionStatus::Exited,
                Some(Utc::now()),
                Some(SessionEndReason::StartFailed),
            )
            .await
            {
                tracing::error!(
                    session_id = %session.id, %update_err,
                    "failed to mark session exited after a failed baseline write"
                );
            }
            return Err(EngineError::ReadOnlyBaseline {
                stage: stage_name.to_string(),
                role: role.to_string(),
                reason: err.to_string(),
            });
        }
        self.events_notify.notify_waiters();

        // Recorded before the session starts, for the same ordering reason
        // as the human message below: this is the one line on the timeline
        // that says the turn picked up where an earlier one left off.
        if let Some(resume) = resume {
            let message = format!(
                "resuming adapter session {} from session {}, whose turn was interrupted because {}",
                resume.adapter_session_id,
                resume.previous_session_id,
                resume.describe()
            );
            if let Err(err) = events::append(
                &self.pool,
                &session.id,
                EventType::SessionNote,
                json!({ "kind": "resume", "message": message }),
            )
            .await
            {
                tracing::error!(session_id = %session.id, %err, "failed to record a resume note");
            } else {
                self.events_notify.notify_waiters();
            }
        }

        // Recorded *before* starting the session, not after — once
        // started, the drain task can react and append its own events
        // (session_meta, the reply) at any point, on any thread, so
        // recording first is what guarantees this event always sorts
        // ahead of anything the session produces, regardless of
        // scheduling (see `send_message`'s identical reasoning). Only
        // when `prompt` came from human-typed `input`, not a
        // `prompt_file` — a template-rendered system prompt (a
        // coder/reviewer turn's own instructions, say) isn't something a
        // human said, so it doesn't belong in the human side of the
        // conversation the way a chat task's initial message does.
        // Best-effort: a transient DB failure here shouldn't block
        // starting the turn.
        if prompt_file.is_none()
            && resume.is_none()
            && let Err(err) = events::append(
                &self.pool,
                &session.id,
                EventType::HumanMessage,
                json!({ "text": prompt }),
            )
            .await
        {
            tracing::error!(session_id = %session.id, %err, "failed to record human message event");
        }

        // A stage with an empty `on:` map (chat, §5.4) never concludes — it
        // just keeps accepting further live messages into the same session
        // indefinitely. Everything else is single-shot: it completes once the
        // agent reports and its turn ends (#90), which is what the watcher
        // below waits for — computed once here rather than at each site
        // separately, so the two decisions can't diverge.
        let session_kind = if stage_def.on.is_empty() {
            SessionKind::Standing
        } else {
            SessionKind::SingleShot
        };

        let started = match resume {
            Some(resume) => {
                self.session_manager
                    .resume(
                        &session.id,
                        &resume.adapter_session_id,
                        &prompt,
                        &resolved.role_config,
                        session_kind,
                    )
                    .await
            }
            None => {
                self.session_manager
                    .start(&session.id, &prompt, &resolved.role_config, session_kind)
                    .await
            }
        };
        if let Err(err) = started {
            // The session row was just created `Active` above; without
            // this, a spawn failure here leaves it Active forever (nothing
            // else in this module ever transitions it), wedging the task
            // since workflow_state was already committed to this stage by
            // the caller before enter_stage ran (§ review on PR #35).
            //
            // This function returns `Err(EngineError::Session(err))` below,
            // which is exactly the signal X-4 (issue #61) needs: whichever
            // caller entered this stage — `create_task`/`start_task` for an
            // entry-stage `agent_turn`, or `finish_detached`/
            // `finish_turn`'s catch-all `Err(err)` arm
            // when a prior stage's `advance_from_stage` re-enters this one —
            // marks the *task* stuck with this error, so it's queryable from
            // `choco task status`/`GET /tasks/{id}` rather than only
            // discoverable in this log line and the session's own
            // `end_reason: "start_failed"`.
            tracing::error!(task_id, session_id = %session.id, %err, "failed to start session for agent_turn");
            if let Err(update_err) = sessions::update_status(
                &self.pool,
                &session.id,
                SessionStatus::Exited,
                Some(Utc::now()),
                Some(SessionEndReason::StartFailed),
            )
            .await
            {
                tracing::error!(
                    session_id = %session.id, %update_err,
                    "failed to mark session exited after a failed session start"
                );
            }
            return Err(EngineError::Session(err));
        }

        // A standing-open session (chat) has no outcome to ever watch for.
        // This is also why the loader rejects `capture:` on such a stage:
        // with no watcher there is no moment at which it could be taken.
        if session_kind == SessionKind::SingleShot {
            self.spawn_turn_watcher(
                task_id.to_string(),
                Arc::clone(definition),
                stage_name.to_string(),
                capture,
                session.id,
            );
        }
        Ok(())
    }

    /// The `worktree_baseline` payload for a read-only role's turn (#172):
    /// a fresh snapshot of `cwd`, or — for a resumed turn — the baseline of
    /// the session being resumed, so work done before an interruption is still
    /// caught. Any failure is an error: the turn must not start unchecked.
    async fn read_only_baseline(
        &self,
        stage: &str,
        role: &str,
        cwd: &std::path::Path,
        resumed_session: Option<&str>,
    ) -> Result<Value, EngineError> {
        let fail = |reason: String| EngineError::ReadOnlyBaseline {
            stage: stage.to_string(),
            role: role.to_string(),
            reason,
        };
        let (cwd, head, branch, sha, entries, status, inherited_from) = match resumed_session {
            Some(previous) => {
                let prev = events::worktree_baseline_for_session(&self.pool, previous)
                    .await
                    .map_err(|err| {
                        fail(format!(
                            "could not read the worktree baseline of session {previous} being \
                             resumed: {err}"
                        ))
                    })?
                    .ok_or_else(|| {
                        fail(format!(
                            "session {previous} being resumed has no worktree baseline; check \
                             the worktree, then choco task retry --fresh"
                        ))
                    })?;
                for key in ["cwd", "head", "branch", "status_sha256"] {
                    if !prev.get(key).is_some_and(Value::is_string) {
                        return Err(fail(format!(
                            "the worktree baseline of session {previous} being resumed has no \
                             '{key}'; check the worktree, then choco task retry --fresh"
                        )));
                    }
                }
                if !prev.get("status_entries").is_some_and(Value::is_u64) {
                    return Err(fail(format!(
                        "the worktree baseline of session {previous} being resumed has no \
                         'status_entries'; check the worktree, then choco task retry --fresh"
                    )));
                }
                let get = |key: &str| prev.get(key).cloned().unwrap_or(Value::Null);
                (
                    get("cwd"),
                    get("head"),
                    get("branch"),
                    get("status_sha256"),
                    get("status_entries"),
                    get("status"),
                    json!(previous),
                )
            }
            None => {
                let snap = worktree::snapshot(cwd)
                    .await
                    .map_err(|err| fail(err.to_string()))?;
                (
                    json!(cwd.to_string_lossy()),
                    json!(snap.head),
                    json!(snap.branch),
                    json!(snap.status_sha256),
                    json!(snap.status_entries),
                    json!(snap.status_preview),
                    Value::Null,
                )
            }
        };
        let message = format!(
            "worktree baseline for read-only role '{role}': HEAD {} on {}, {} status entries",
            short_sha(head.as_str().unwrap_or("")),
            branch_label(branch.as_str().unwrap_or("")),
            entries
        );
        Ok(json!({
            "stage": stage,
            "role": role,
            "cwd": cwd,
            "head": head,
            "branch": branch,
            "status_sha256": sha,
            "status_entries": entries,
            "status": status,
            "inherited_from": inherited_from,
            "message": message,
        }))
    }

    /// The post-turn check for a `read_only` role (#172), on a turn that
    /// completed. Returns `true` when the turn may be applied (not read-only,
    /// or the worktree is unchanged) and `false` once the task has been
    /// parked as stuck. A check that can't run parks the task too: it never
    /// passes silently.
    async fn read_only_check_passes(
        &self,
        task_id: &str,
        definition: &WorkflowDefinition,
        stage_name: &str,
        session_id: &str,
    ) -> bool {
        match self
            .read_only_verdict(task_id, definition, stage_name, session_id, true)
            .await
        {
            ReadOnlyVerdict::Clean => true,
            ReadOnlyVerdict::Violation(reason) | ReadOnlyVerdict::Unverified(reason) => {
                self.mark_stuck(task_id, &reason, false).await;
                false
            }
        }
    }

    /// Parks a task whose turn did not complete (crash, no report, lingering
    /// process, usage limit, reaper, daemon stop), after comparing a
    /// read-only role's worktree with its baseline (#172): a turn that ended
    /// abnormally may still have changed the worktree, and a plain retry of a
    /// session that can't be resumed would otherwise take the dirty state as
    /// its new baseline. A violation, or a check that can't run, is added to
    /// the stuck reason.
    async fn park_incomplete_turn(
        &self,
        task_id: &str,
        definition: &WorkflowDefinition,
        stage_name: &str,
        session_id: &str,
        reason: &str,
    ) {
        let reason = match self
            .read_only_verdict(task_id, definition, stage_name, session_id, false)
            .await
        {
            ReadOnlyVerdict::Clean => reason.to_string(),
            ReadOnlyVerdict::Violation(found) | ReadOnlyVerdict::Unverified(found) => {
                format!("{reason}; {found}")
            }
        };
        self.mark_stuck(task_id, &reason, false).await;
    }

    /// Compares a read-only role's worktree with the baseline of `session_id`.
    /// Records the `worktree_changed` event on a difference; parks nothing.
    pub(super) async fn read_only_verdict(
        &self,
        task_id: &str,
        definition: &WorkflowDefinition,
        stage_name: &str,
        session_id: &str,
        outcome_pending: bool,
    ) -> ReadOnlyVerdict {
        let subject = match definition.stages.get(stage_name).map(|s| &s.kind) {
            Some(StageKind::AgentTurn { role, .. }) => format!("read-only role '{role}'"),
            _ => format!("stage '{stage_name}'"),
        };
        // A turn that ended abnormally never produced an outcome, so only a
        // turn that did can say its outcome was not applied.
        let unverified = |error: String| {
            let tail = if outcome_pending {
                "The turn's outcome was not applied: inspect the worktree, then choco task retry"
            } else {
                "Inspect the worktree, then choco task retry"
            };
            format!(
                "could not verify that {subject} left the worktree unchanged in \
                 stage '{stage_name}': {error}. {tail}"
            )
        };
        let Some(role_def) = definition
            .stages
            .get(stage_name)
            .and_then(|s| match &s.kind {
                StageKind::AgentTurn { role, .. } => definition.roles.get(role),
                _ => None,
            })
        else {
            return ReadOnlyVerdict::Unverified(unverified(format!(
                "stage '{stage_name}' has no agent_turn role in the workflow definition"
            )));
        };
        if !role_def.read_only {
            return ReadOnlyVerdict::Clean;
        }
        let role = match definition.stages.get(stage_name).map(|s| &s.kind) {
            Some(StageKind::AgentTurn { role, .. }) => role.as_str(),
            _ => return ReadOnlyVerdict::Unverified(unverified("stage lost its role".into())),
        };

        let outcome: Result<Option<String>, String> = async {
            let baseline = events::worktree_baseline_for_session(&self.pool, session_id)
                .await
                .map_err(|err| format!("could not read the worktree baseline: {err}"))?
                .ok_or_else(|| "this session has no worktree baseline".to_string())?;
            let field = |key: &str| {
                baseline
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("the worktree baseline has no '{key}'"))
            };
            let (cwd, head, branch, sha) = (
                field("cwd")?,
                field("head")?,
                field("branch")?,
                field("status_sha256")?,
            );
            let before_entries = baseline
                .get("status_entries")
                .and_then(Value::as_u64)
                .ok_or_else(|| "the worktree baseline has no 'status_entries'".to_string())?;
            let now = worktree::snapshot(std::path::Path::new(&cwd))
                .await
                .map_err(|err| err.to_string())?;

            let mut changes = Vec::new();
            let mut parts = Vec::new();
            if now.head != head {
                parts.push(format!(
                    "HEAD {} → {}",
                    short_sha(&head),
                    short_sha(&now.head)
                ));
                changes.push(json!({"field": "head", "before": head, "after": now.head}));
            }
            if now.branch != branch {
                parts.push(format!(
                    "branch {} → {}",
                    branch_label(&branch),
                    branch_label(&now.branch)
                ));
                changes.push(json!({"field": "branch", "before": branch, "after": now.branch}));
            }
            if now.status_sha256 != sha {
                // Equal counts with different bytes: the entries may be the
                // same paths with new contents (or renamed ones); say so
                // rather than claim the status moved.
                if before_entries == now.status_entries as u64 {
                    parts.push(format!(
                        "git status or file contents changed ({} entries)",
                        now.status_entries
                    ));
                } else {
                    parts.push(format!(
                        "git status changed ({} entries)",
                        now.status_entries
                    ));
                }
                changes.push(json!({
                    "field": "status",
                    "before": before_entries,
                    "after": now.status_entries,
                    "before_sha256": sha,
                    "after_sha256": now.status_sha256,
                }));
            }
            if parts.is_empty() {
                return Ok(None);
            }
            let reason = format!(
                "read-only role '{role}' changed the worktree in stage '{stage_name}': {}. \
                 Nothing was reverted: inspect the worktree, reset it, then choco task retry",
                parts.join("; ")
            );
            // The watcher and the restart sweep can both look at the same
            // session; record the violation once.
            let already = events::list_for_session(&self.pool, session_id)
                .await
                .map(|events| {
                    events
                        .iter()
                        .any(|e| e.event_type == EventType::WorktreeChanged)
                })
                .unwrap_or_else(|err| {
                    // A failed read errs toward recording a second event.
                    tracing::error!(task_id, session_id, %err,
                        "could not list session events to dedupe worktree_changed");
                    false
                });
            if already {
                return Ok(Some(reason));
            }
            match events::append(
                &self.pool,
                session_id,
                EventType::WorktreeChanged,
                json!({
                    "stage": stage_name,
                    "role": role,
                    "changes": changes,
                    "status_entries": now.status_entries,
                    "status": now.status_preview,
                    "message": reason,
                }),
            )
            .await
            {
                Ok(_) => self.events_notify.notify_waiters(),
                Err(err) => tracing::error!(
                    task_id, session_id, %err,
                    "failed to record the worktree_changed event; parking the task anyway"
                ),
            }
            Ok(Some(reason))
        }
        .await;

        match outcome {
            Ok(None) => ReadOnlyVerdict::Clean,
            Ok(Some(reason)) => ReadOnlyVerdict::Violation(reason),
            Err(error) => ReadOnlyVerdict::Unverified(unverified(error)),
        }
    }

    /// Watches a single-shot `agent_turn`'s `session` for completion, takes
    /// its `capture:` if it declared one, and auto-advances.
    ///
    /// Without a `capture:` the outcome is `done`, which is what §5.2 says a
    /// plain single-shot turn emits. With `capture: json` it is instead read
    /// from the reply's reserved `outcome` key (#45) — one mechanism serving
    /// both the `on:` transition and the values later stages template in,
    /// rather than a separate verdict channel.
    ///
    /// A crashed/non-zero exit is logged and left for a human to notice
    /// rather than guessing an outcome the stage's `on:` map was never
    /// designed to receive.
    pub(super) fn spawn_turn_watcher(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        capture: Option<Capture>,
        session_id: String,
    ) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match sessions::get(&engine.pool, &session_id).await {
                    // `Idle` is also what the idle reaper leaves behind
                    // when it force-closes a stalled turn's stdin
                    // (session.rs's `drain_session`) — indistinguishable
                    // from a turn finishing on its own by `status` alone,
                    // so `end_reason` is what actually decides whether
                    // this was a real completion.
                    // Ordered ahead of every status arm below, because it
                    // is the one reason that decides the outcome on its own
                    // (#69): a cancelled run lands on `Exited` normally, but
                    // a turn that finished cleanly in the instant before
                    // the kill landed lands on `Idle` — and the `Idle` arm
                    // below would `break` and advance a task the operator
                    // had already stopped. `advance_from_stage`'s guard
                    // would still refuse that transition, so this is the
                    // early, quiet exit rather than the thing that makes
                    // cancel correct.
                    Ok(Some(run)) if run.end_reason == Some(SessionEndReason::Cancelled) => {
                        tracing::info!(
                            task_id,
                            session_id,
                            "session was cancelled; not auto-advancing"
                        );
                        return;
                    }
                    // Either status: a reaper-closed turn that exited cleanly
                    // is `Idle`, one whose process then had to be killed is
                    // `Exited` (#90).
                    Ok(Some(run)) if run.end_reason == Some(SessionEndReason::Reaped) => {
                        tracing::warn!(
                            task_id,
                            session_id,
                            "session was force-closed by the idle reaper before completing its turn; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &format!(
                                    "stage '{stage_name}': the agent turn was force-closed by \
                                     the idle reaper before completing; 'choco task retry' will \
                                     resume it"
                                ),
                            )
                            .await;
                        return;
                    }
                    // #92: the turn was cut off from outside, not by
                    // anything the agent did. Parked like any other
                    // incomplete turn, but named as what it is, because
                    // this is the one stuck reason whose recovery is
                    // different: retry continues the session instead of
                    // starting another one over the same worktree.
                    Ok(Some(run)) if run.end_reason == Some(SessionEndReason::Interrupted) => {
                        tracing::warn!(
                            task_id,
                            session_id,
                            "session was interrupted by a usage limit; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &format!(
                                    "stage '{stage_name}': the agent's turn was interrupted by a \
                                     usage limit before it could report; 'choco task retry' will \
                                     resume it"
                                ),
                            )
                            .await;
                        return;
                    }
                    // #84: the daemon stopped under this turn (graceful
                    // shutdown records it itself; the startup park sweep
                    // records it after a crash). Whatever the status.
                    Ok(Some(run)) if run.end_reason == Some(SessionEndReason::DaemonStopped) => {
                        tracing::warn!(
                            task_id,
                            session_id,
                            "the daemon stopped during this turn; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &agent_reason(&stage_name),
                            )
                            .await;
                        return;
                    }
                    Ok(Some(run)) if run.status == SessionStatus::Idle => break,
                    // #90: the two ways `drain_session` ends a single-shot
                    // turn it could not treat as complete. Each gets its own
                    // reason, since "exited without completing" would send a
                    // human looking for a crash that never happened.
                    Ok(Some(run))
                        if run.status == SessionStatus::Exited
                            && run.end_reason == Some(SessionEndReason::NoReport) =>
                    {
                        tracing::warn!(
                            task_id,
                            session_id,
                            "session ended without reporting its outcome; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &format!(
                                    "stage '{stage_name}': the agent's turn ended without \
                                     calling report_outcome"
                                ),
                            )
                            .await;
                        return;
                    }
                    Ok(Some(run))
                        if run.status == SessionStatus::Exited
                            && run.end_reason == Some(SessionEndReason::Lingered) =>
                    {
                        tracing::warn!(
                            task_id,
                            session_id,
                            "session's process kept running after its turn ended and was killed; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &format!(
                                    "stage '{stage_name}': the agent process kept running after \
                                     its turn ended and was killed; work it started may be \
                                     incomplete"
                                ),
                            )
                            .await;
                        return;
                    }
                    Ok(Some(run)) if run.status == SessionStatus::Exited => {
                        tracing::warn!(
                            task_id,
                            session_id,
                            "session exited without completing its turn cleanly; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &format!(
                                    "stage '{stage_name}': the agent process exited without \
                                     completing its turn"
                                ),
                            )
                            .await;
                        return;
                    }
                    Ok(Some(_)) => {}
                    // The session row is gone, which means the task itself was
                    // deleted — there is no task left to mark stuck.
                    Ok(None) => {
                        tracing::error!(
                            task_id,
                            session_id,
                            "session disappeared while watching for turn completion; not auto-advancing"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::error!(
                            task_id, session_id, %err,
                            "failed to poll session while watching for turn completion; not auto-advancing"
                        );
                        engine
                            .park_incomplete_turn(
                                &task_id,
                                &definition,
                                &stage_name,
                                &session_id,
                                &format!(
                                    "stage '{stage_name}': lost track of the agent turn: {err}"
                                ),
                            )
                            .await;
                        return;
                    }
                }
                tokio::time::sleep(TURN_WATCH_INTERVAL).await;
            }
            engine
                .finish_turn(&task_id, &definition, &stage_name, capture, &session_id)
                .await;
        });
    }

    /// Applies a completed turn's capture and outcome. Runs detached, like
    /// `finish_detached`, so there is nothing to
    /// return a failure to — it is logged and the task parks.
    pub(super) async fn finish_turn(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        capture: Option<Capture>,
        session_id: &str,
    ) {
        // #172: before anything is fetched, resolved or routed, a read-only
        // role's worktree is compared with its baseline.
        if !self
            .read_only_check_passes(task_id, definition, stage_name, session_id)
            .await
        {
            return;
        }

        // Issue #73: an explicit `report_outcome` tool call, if the agent
        // made one, is unambiguous where a reply is guesswork, so it's
        // fetched first and preferred whenever a capturing stage has one.
        // Fetched unconditionally — not only for `capture: json` stages — so
        // a report made on a stage that doesn't route on it is still visible
        // on the timeline instead of silently discarded (see the
        // `capture.is_some() || report.is_some()` gate below).
        //
        // A fetch failure here degrades to the text-reply path below rather
        // than parking outright, unlike `final_assistant_text_for_run`'s own
        // error arm: that path is the *only* way to learn a turn's outcome,
        // where this is one of two, and the other (the reply) is untouched
        // by this specific failure. Degrading isn't the same as swallowing
        // it, though — `report_fetch_note` carries it onto the
        // `turn_outcome` event's `note` below, not just into the log, and
        // the write-gate is widened so this alone is enough to write one
        // even on a no-capture stage that would otherwise see nothing.
        let (report, report_fetch_note) =
            match events::last_report_outcome_for_session(&self.pool, session_id).await {
                Ok(report) => (report, None),
                Err(err) => {
                    tracing::error!(
                        task_id, session_id, stage = stage_name, %err,
                        "could not read a turn's report_outcome tool call back; falling back to \
                         its reply"
                    );
                    (
                        None,
                        Some(format!(
                            "could not check for a report_outcome call ({err}); used the \
                             turn's reply instead"
                        )),
                    )
                }
            };

        // Issue #73/review: a report only *routes* when the stage declares
        // `capture: json` — that's the one rule this whole feature promises
        // ("a stage routes on the agent's own verdict iff it declares
        // `capture: json`"). `capture: text` and no-`capture:` stages must
        // treat an agent's report exactly like `capture: None` already did
        // before this match existed: worth a note, never a transition.
        // Without this gate, a `capture: text` stage whose agent calls the
        // tool anyway would be routed — or parked — on a verdict its `on:`
        // map was never meant to receive.
        let routing_report = match capture {
            Some(Capture::Json) => report.as_ref(),
            _ => None,
        };

        // A report made on a stage that doesn't route on it. #90 made every
        // single-shot stage report `done` to complete, so that call is the
        // expected one and not worth a note; only a report of some *other*
        // outcome — one the stage can't act on — is.
        let unroutable_note = (capture != Some(Capture::Json)
            && report.as_ref().is_some_and(|report| {
                report.get("outcome").and_then(Value::as_str) != Some(TURN_DEFAULT_OUTCOME)
            }))
        .then(|| {
            "a report_outcome call was made, but this stage does not route on it \
             (it declares no 'capture: json')"
                .to_string()
        });

        let (captured, outcome, note, source) = match routing_report {
            Some(report) => {
                let serialized = report.to_string();
                // The same `MAX_CAPTURE_BYTES` ceiling `derive_capture`
                // applies to a reply, and the same response to going over
                // it: don't store an oversized value, and don't trust
                // reading an outcome out of it either — fall back to `done`
                // rather than keep only half the safety net.
                if serialized.len() > MAX_CAPTURE_BYTES {
                    let bytes = serialized.len();
                    tracing::warn!(
                        task_id,
                        stage = stage_name,
                        bytes,
                        "stage's report_outcome call too large to capture; not stored"
                    );
                    (
                        None,
                        TURN_DEFAULT_OUTCOME.to_string(),
                        Some(format!(
                            "report not captured: {bytes} bytes exceeds the \
                             {MAX_CAPTURE_BYTES}-byte limit"
                        )),
                        Some("tool"),
                    )
                } else {
                    let (outcome, note) = outcome_from_report(report);
                    (Some(report.clone()), outcome, note, Some("tool"))
                }
            }
            // No routing report: either there's no report at all, or there
            // is one but this stage doesn't route on it (`capture: text`, or
            // no `capture:`). Either way, fall through to what the stage
            // would have done before #73 — and if a report was made but
            // ignored, note that rather than let it vanish.
            None => {
                // Review, #75 round 2: `source` describes where the
                // *outcome* came from (models.rs's own doc on
                // `EventType::TurnOutcome`) — and here it never comes from
                // the report, whether or not one was made: a no-`capture:`
                // stage always advances on `done`, and a `capture: text`
                // stage always reads its own reply. Recording `"tool"`
                // anyway (as this used to) would claim the report drove an
                // outcome it never touched; the note below already says a
                // report existed and was ignored, which is the fact worth
                // recording.
                match capture {
                    None => (
                        None,
                        TURN_DEFAULT_OUTCOME.to_string(),
                        unroutable_note.clone(),
                        None,
                    ),
                    // `capture: text` (or a `capture: json` stage with no
                    // report at all) still reads its own turn back off the
                    // timeline — the same text-parse path as before #73, for
                    // an agent that never calls the tool (or an adapter,
                    // like a future ACP one, that has no tool-call channel
                    // at all).
                    //
                    // The turn's text isn't held anywhere in memory: the
                    // adapter stream is drained straight into `events` by
                    // `drain_session` and dropped, and this watcher only
                    // ever sees `sessions` rows. So the reply is read back
                    // from the timeline.
                    Some(capture) => {
                        match events::final_assistant_text_for_session(&self.pool, session_id).await
                        {
                            Ok(reply) => {
                                let reply = unwrap_code_fence(reply.trim());
                                let (captured, capture_note) =
                                    derive_agent_reply_capture(capture, reply, task_id, stage_name);
                                let (outcome, outcome_note) =
                                    turn_outcome(capture, captured.as_ref());
                                let note = [capture_note.or(outcome_note), unroutable_note.clone()]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join("; ");
                                let note = (!note.is_empty()).then_some(note);
                                (captured, outcome, note, Some("reply"))
                            }
                            // Nothing to capture and no basis for a verdict,
                            // so this does not fall through to a default
                            // outcome — a task that can't read its own turn
                            // back parks for a human.
                            Err(err) => {
                                tracing::error!(
                                    task_id, session_id, stage = stage_name, %err,
                                    "could not read a turn's reply back to capture it; not \
                                     auto-advancing"
                                );
                                let note = [
                                    Some(format!("the turn's reply could not be read back: {err}")),
                                    unroutable_note.clone(),
                                    report_fetch_note,
                                ]
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>()
                                .join("; ");
                                self.append_turn_outcome_event(
                                    task_id,
                                    session_id,
                                    json!({
                                        "stage": stage_name,
                                        "capture": capture_label(Some(capture)),
                                        "outcome": Value::Null,
                                        "applied": false,
                                        "note": note,
                                        "source": Value::Null,
                                    }),
                                )
                                .await;
                                self.mark_stuck(
                                    task_id,
                                    &format!(
                                        "stage '{stage_name}': the turn's reply could not be \
                                         read back: {err}"
                                    ),
                                    false,
                                )
                                .await;
                                return;
                            }
                        }
                    }
                }
            }
        };

        // `expected_stage` below catches a task that has *left* this stage,
        // but not one that left and came back: re-entering opens a new
        // `session`, and a late watcher for the superseded one would pass
        // that check and overwrite the fresh capture with a stale verdict.
        // Advisory only, like poll's `still_in_stage` — it runs outside the
        // lock, and nothing can produce that interleaving today (nothing
        // moves a task out of an `agent_turn` while its run is live), so this
        // is the invariant announcing itself rather than a known case.
        if !self
            .is_current_run_for_stage(task_id, stage_name, session_id)
            .await
        {
            tracing::warn!(
                task_id,
                session_id,
                stage = stage_name,
                "discarded a turn's outcome: its stage has since started a newer run"
            );
            return;
        }

        // `expected_stage` matters even though a turn holds its stage open:
        // a human can close or resume the task between the run going idle
        // and this write, and the capture is keyed by the stage the check
        // confirms is still current.
        let applied = self
            .advance_from_stage(task_id, definition, &outcome, Some(stage_name), captured)
            .await;

        let applied_note = match &applied {
            Ok(()) => {
                tracing::debug!(
                    task_id,
                    session_id,
                    stage = stage_name,
                    outcome,
                    "turn completed; advanced"
                );
                None
            }
            // Deliberately parked, not broken — the same classification
            // `finish_detached` uses. A reviewer stage that declares only
            // `approved`/`changes_requested` and whose reply carried neither
            // lands here, which is the intended place for a human to pick it
            // up rather than the engine inventing a transition.
            Err(EngineError::UnknownOutcome { stage, outcome }) => {
                tracing::info!(
                    task_id,
                    stage,
                    outcome,
                    "turn parked: its outcome has no 'on:' edge"
                );
                // `note` (from the outer match) already says when `outcome`
                // itself was a fallback — e.g. the reply carried no
                // 'outcome' key and this advanced with 'done' anyway — so
                // it's folded into the reason a human sees on the task
                // rather than just on this turn's own event.
                let reason = match &note {
                    Some(note) => format!(
                        "stage '{stage}': turn outcome '{outcome}' has no 'on:' edge ({note})"
                    ),
                    None => format!("stage '{stage}': turn outcome '{outcome}' has no 'on:' edge"),
                };
                self.mark_stuck(task_id, &reason, false).await;
                Some(format!(
                    "parked: stage '{stage}' has no 'on:' edge for '{outcome}'"
                ))
            }
            Err(EngineError::StageMovedOn { expected, actual }) => {
                tracing::info!(
                    task_id,
                    expected,
                    actual,
                    outcome,
                    "discarded a turn's outcome: the task had already left that stage"
                );
                Some(format!(
                    "not applied: the task had already left '{expected}' for '{actual}'"
                ))
            }
            // A turn that completed in the same instant it was cancelled.
            // The note goes on the `turn_outcome` event, so the timeline
            // says why the verdict wasn't applied rather than leaving a
            // reader to infer it from the task's status.
            Err(EngineError::TaskCancelled(_)) => {
                tracing::info!(
                    task_id,
                    stage = stage_name,
                    outcome,
                    "discarded a turn's outcome: the task was cancelled"
                );
                Some("not applied: the task was cancelled".to_string())
            }
            Err(err) => {
                tracing::error!(
                    task_id, stage = stage_name, outcome, %err,
                    "task wedged: its turn completed but the transition failed"
                );
                // See `finish_detached`'s identical catch-all for why
                // this blames whichever stage actually failed rather than
                // always `stage_name`.
                let blamed = self.stage_to_blame(task_id, stage_name).await;
                let reason = if blamed == stage_name {
                    format!("stage '{stage_name}': turn completed but the transition failed: {err}")
                } else {
                    format!(
                        "stage '{blamed}': could not be entered after '{stage_name}' completed: {err}"
                    )
                };
                self.mark_stuck(
                    task_id,
                    &reason,
                    matches!(err, EngineError::Template { .. }),
                )
                .await;
                Some(format!("not applied: {err}"))
            }
        };

        // Written *after* the advance, and carrying whether it was applied,
        // so the entry can't claim a transition that was rejected — the park
        // this feature's lenient fallback relies on is exactly the case where
        // the outcome is computed but deliberately not taken.
        //
        // The cost of that ordering: `advance_from_stage` records the next
        // stage's `stage_entered` first, so on the timeline this entry sits
        // just *after* the transition it explains (and after a fast next
        // stage's own output). `shell_output` is written before its advance
        // and so reads the other way round. Accepted deliberately: an entry
        // that is one line late is a smaller problem than one that asserts a
        // transition which never happened.
        // Issue #73: also written for a no-capture stage that received a
        // report, or whose report lookup itself failed — that's the
        // "recorded, not silently dropped" half of the rule.
        // `capture.is_some()` alone was sufficient before the report existed
        // as a second (and its lookup failing, a third) possible reason to
        // write this event; without `report_fetch_note.is_some()` here, a
        // report-lookup failure on a no-capture stage would reach nothing
        // but the log.
        //
        // #90 narrowed "received a report" to "received a report it can't act
        // on": every single-shot stage now reports `done` to complete, and a
        // `turn_outcome` for each of those would say nothing the stage trail
        // doesn't.
        if capture.is_some() || unroutable_note.is_some() || report_fetch_note.is_some() {
            // Only added when the stage actually parked: an author whose
            // `capture: text` turn routed fine through `on: { done: … }`
            // doesn't need to be told about `capture: json`. Added *here*
            // rather than suppressed later, so it can't swallow a note that
            // was explaining something else — an oversized reply that wasn't
            // stored at all is the one that must always survive.
            let text_hint = (capture == Some(Capture::Text) && applied.is_err()).then(|| {
                "'capture: text' keeps the reply but carries no verdict; use 'capture: json' \
                 to route on an 'outcome' key"
                    .to_string()
            });
            let note = [note, applied_note, text_hint, report_fetch_note]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let note = (!note.is_empty()).then(|| note.join("; "));
            self.append_turn_outcome_event(
                task_id,
                session_id,
                json!({
                    "stage": stage_name,
                    "capture": capture_label(capture),
                    "outcome": outcome,
                    "applied": applied.is_ok(),
                    "note": note,
                    "source": source,
                }),
            )
            .await;
        }
    }

    /// Whether `session_id` is still the newest run of `stage_name`.
    ///
    /// The `Err` arm errs towards proceeding: this only narrows a window
    /// nothing can reach today, and refusing to advance because a *check*
    /// failed would strand a task whose turn genuinely completed.
    ///
    /// Two caveats, both unreachable today and both deliberate. A `false`
    /// answer returns without a timeline entry, unlike the template failure
    /// above — it means two runs of one stage overlapped, which no path
    /// produces. And `get_current_for_stage` tie-breaks on a random UUID, so
    /// two runs started within one timestamp tick could pick the wrong
    /// "current" one and discard a legitimate outcome; that needs the same
    /// impossible overlap to happen at all.
    async fn is_current_run_for_stage(
        &self,
        task_id: &str,
        stage_name: &str,
        session_id: &str,
    ) -> bool {
        match sessions::get_current_for_stage(&self.pool, task_id, stage_name).await {
            Ok(Some(current)) => current.id == session_id,
            // The run this watcher is for exists, so no row at all means the
            // task was deleted underneath it.
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    task_id, session_id, stage = stage_name, %err,
                    "could not confirm a completed turn is its stage's current run; advancing anyway"
                );
                true
            }
        }
    }

    async fn append_turn_outcome_event(&self, task_id: &str, session_id: &str, payload: Value) {
        match events::append(&self.pool, session_id, EventType::TurnOutcome, payload).await {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, session_id, %err,
                "failed to record turn outcome event"
            ),
        }
    }
}
