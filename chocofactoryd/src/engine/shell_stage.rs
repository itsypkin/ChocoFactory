use super::runners::DetachedKind;
use super::stage_capture::{MAX_CAPTURE_BYTES, derive_capture};
use super::*;

/// How much of a command's stdout/stderr goes onto the timeline. Small on
/// purpose: this is a human-facing breadcrumb. Arbitrary command output is
/// not kept beyond it; only a capture the stage's `capture:` accepted is, and
/// that lives in two places: the stage-result event of a departure that
/// committed, and `workflow_state.payload.stages` (the latest value per
/// stage).
pub(super) const EVENT_OUTPUT_TAIL_BYTES: usize = 2048;

/// Logs a poll attempt whose process group outlived its kill.
///
/// `run_shell_stage` warns on the same condition, and an operator grepping
/// logs for escaped process groups should find both kinds — a poll is if
/// anything the likelier source, since it can kill an attempt on every
/// interval for as long as its budget lasts.
pub(super) fn warn_if_escaped(task_id: &str, stage_name: &str, outcome: &shell::ShellOutcome) {
    if outcome.escaped {
        tracing::warn!(
            task_id,
            stage = stage_name,
            "a poll attempt was killed but its process group could not be confirmed dead"
        );
    }
}

/// A duration as whole milliseconds, saturating rather than wrapping — a
/// nonsense number on the timeline is worse than a clamped one.
pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn elapsed_ms(since: std::time::Instant) -> u64 {
    duration_ms(since.elapsed())
}

/// The trailing `EVENT_OUTPUT_TAIL_BYTES` of `text`, trimmed. The *tail*
/// rather than the head because a failing command's actual error is
/// almost always the last thing it printed.
pub(super) fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= EVENT_OUTPUT_TAIL_BYTES {
        return trimmed.to_string();
    }
    // Walk back to a char boundary so a multi-byte char isn't split.
    let mut start = trimmed.len() - EVENT_OUTPUT_TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

/// Checks the stdout of a successful `open_review` command and returns the
/// capture to record: the printed object, with `label` filled in as
/// `#<number>` when it was absent. Everything else the script printed is kept
/// as printed. `Err` is the reason the output is not a review reference.
///
/// This is the whole publication contract: the engine checks the shape of the
/// answer, never what the URL points at, so no forge is special.
pub(super) fn validate_review_output(stdout: &str) -> Result<Value, String> {
    let trimmed = stdout.trim();
    // Before parsing, so an oversized document is refused for its size and
    // never copied into a parse tree.
    if trimmed.len() > MAX_CAPTURE_BYTES {
        return Err(format!(
            "output exceeds the {MAX_CAPTURE_BYTES}-byte capture limit"
        ));
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|err| format!("output must be valid JSON: {err}"))?;
    let Value::Object(mut object) = value else {
        return Err("output must be a JSON object".to_string());
    };
    let number = object
        .get("number")
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .ok_or("`number` must be a positive integer")?;
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or("`url` must be a string")?;
    let scheme_len = ["http://", "https://"]
        .iter()
        .find(|scheme| {
            url.get(..scheme.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
        })
        .map(|scheme| scheme.len());
    if scheme_len.is_none_or(|len| url.len() == len) {
        return Err(
            "`url` must start with http:// or https:// and have a nonempty remainder".to_string(),
        );
    }
    if url.chars().any(char::is_whitespace) {
        return Err("`url` must contain no whitespace".to_string());
    }
    if let Some(token) = first_inexact_number(trimmed) {
        return Err(format!(
            "number {token} does not fit a 64-bit integer or double exactly, so storing \
             it would change it; print it as a string"
        ));
    }
    match object.get("label") {
        None => {
            object.insert("label".to_string(), Value::String(format!("#{number}")));
        }
        Some(Value::String(label)) if !label.trim().is_empty() && !label.contains(['\n', '\r']) => {
        }
        Some(_) => return Err("`label` must be a nonempty single-line string".to_string()),
    }
    Ok(Value::Object(object))
}

/// The first number token in `json` (a document `serde_json` already parsed)
/// that parsing into a [`Value`] would change: one that is neither an `i64` or
/// `u64` nor the shortest spelling of an `f64`. `Value` holds no other number,
/// so such a token would be rounded; a publisher's output is refused instead.
/// Respellings of the same value (`1.10` as `1.1`, `1e5` as `100000.0`) pass.
///
/// Reads the text rather than the `Value`: serde_json's `arbitrary_precision`
/// feature would keep the digits, but it is process-wide, turns an object whose
/// first key is `$serde_json::private::Number` into a number, and changes how
/// every other capture parses.
fn first_inexact_number(json: &str) -> Option<&str> {
    let bytes = json.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    i += 1;
                }
                let token = &json[start..i];
                if !number_survives_parsing(token) {
                    return Some(token);
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn number_survives_parsing(token: &str) -> bool {
    if token.parse::<i64>().is_ok() || token.parse::<u64>().is_ok() {
        return true;
    }
    let Ok(float) = token.parse::<f64>() else {
        return false;
    };
    float.is_finite() && decimal_parts(token) == decimal_parts(&format!("{float:e}"))
}

/// `(negative, digits, exponent)` with the value `0.<digits> × 10^exponent`
/// ignoring leading and trailing zeros, so spellings of one value compare
/// equal; zero is `(false, "", 0)`. `None` for a token this can't read.
fn decimal_parts(token: &str) -> Option<(bool, String, i64)> {
    let (negative, rest) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let exponent = exponent.checked_add(whole.len() as i64)?;
    let significant = digits.trim_start_matches('0');
    let exponent = exponent - (digits.len() - significant.len()) as i64;
    let significant = significant.trim_end_matches('0');
    if significant.is_empty() {
        return Some((false, String::new(), 0));
    }
    Some((negative, significant.to_string(), exponent))
}

/// How a command is named on the timeline. A `script_file` shows its path;
/// there's no meaningful "command line" to display for one.
pub(super) fn describe_command(command: &ShellCommand) -> String {
    match command {
        ShellCommand::Inline(line) => line.clone(),
        ShellCommand::ScriptFile(path) => path.display().to_string(),
    }
}

impl WorkflowEngine {
    /// Starts a `shell` stage's command (§5.2) and returns immediately; the
    /// outcome arrives later, from the detached runner below.
    ///
    /// Running the command inline here instead would deadlock the task
    /// permanently: `enter_stage` is called from inside the per-task lock
    /// held by `advance`/`start_task`, the runner has to call `advance` to
    /// report its outcome, and `tokio::sync::Mutex` is not reentrant. The
    /// `agent_turn` path has the same constraint and resolves it the same
    /// way, via `spawn_turn_watcher`.
    pub(super) async fn enter_shell(
        self: &Arc<Self>,
        entry: &StageEntry<'_>,
    ) -> Result<(), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name,
            ..
        } = *entry;
        let StageKind::Shell {
            command,
            capture,
            timeout,
            env,
            publishes_review,
        } = &entry.stage_def.kind
        else {
            unreachable!("enter_shell is only called for Shell stages")
        };
        let capture = *capture;
        let timeout = *timeout;
        let publishes_review = *publishes_review;
        let (command, env) = self.render_stage_command(entry, command, env).await?;
        // Resolved here rather than in the spawned task so that a missing
        // task fails the transition that caused it, where the caller can
        // still see the error, instead of only reaching a log line.
        let task = tasks::get(&self.pool, task_id)
            .await?
            .ok_or(EngineError::NoSuchTask)?;
        let cwd = working_dir(&task, definition)?;

        self.spawn_shell_runner(
            task_id.to_string(),
            Arc::clone(definition),
            stage_name.to_string(),
            command,
            capture,
            publishes_review,
            timeout,
            cwd,
            env,
        );
        Ok(())
    }

    /// Deliberately a *synchronous* fn, like `spawn_turn_watcher`. The
    /// spawned future eventually calls `advance` → `enter_stage` →
    /// `enter_shell`, i.e. back to here; discharging `tokio::spawn`'s
    /// `Send` obligation from inside an `async fn` would make that cycle
    /// part of the compiler's auto-trait inference for `enter_shell`'s own
    /// future and fail to resolve. A sync fn's body is checked
    /// independently, which breaks the cycle.
    #[allow(clippy::too_many_arguments)]
    fn spawn_shell_runner(
        self: &Arc<Self>,
        task_id: String,
        definition: Arc<WorkflowDefinition>,
        stage_name: String,
        command: ShellCommand,
        capture: Option<Capture>,
        publishes_review: bool,
        timeout: Option<Duration>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        let engine = Arc::clone(self);
        // Registered so `cancel_task` can abort this runner and kill the
        // command it's running (#69) — a `shell` stage has no `session`,
        // so killing the task's agent session would not reach it.
        let registered_task_id = task_id.clone();
        let spawned = self.spawn_registered_runner(
            &registered_task_id,
            DetachedKind::Shell,
            move |runner_id| async move {
                engine
                    .run_shell_stage(
                        &task_id,
                        &definition,
                        &stage_name,
                        command,
                        capture,
                        publishes_review,
                        timeout,
                        cwd,
                        env,
                    )
                    .await;
                engine.finish_runner(&task_id, runner_id);
                // The last running work of a pausing task ends here (#295).
                engine.settle_pause(&task_id).await;
            },
        );
        if !spawned {
            tracing::warn!(
                task_id = %registered_task_id,
                "daemon is shutting down; shell stage not started, the next start recovers it"
            );
        }
    }

    /// Runs the command, records what it did on the task's timeline, and
    /// advances the task with `done`/`error` (§5.2) carrying any capture.
    #[allow(clippy::too_many_arguments)]
    async fn run_shell_stage(
        self: &Arc<Self>,
        task_id: &str,
        definition: &Arc<WorkflowDefinition>,
        stage_name: &str,
        command: ShellCommand,
        capture: Option<Capture>,
        publishes_review: bool,
        timeout: Option<Duration>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        let described = describe_command(&command);

        // A failure to run the command at all is reported as the stage's
        // `error` outcome rather than dropped: from the workflow's point of
        // view "the command could not be run" and "the command ran and
        // failed" both mean this stage did not succeed, and a task whose
        // `on: error` edge exists should follow it either way. The reason
        // goes on the timeline, since it's the only place an operator would
        // find it.
        let started = std::time::Instant::now();
        let outcome = match shell::run(&command, &cwd, timeout, &env).await {
            Ok(outcome) => outcome,
            Err(err) => {
                // The two variants mean materially different things to
                // whoever reads this: `Spawn` means nothing ran, while `Io`
                // means the command *did* run — possibly for a long time,
                // possibly mutating the working copy — and only reading its
                // output failed. Reporting both as "could not be started"
                // would actively mislead. Two call sites rather than one
                // interpolated message, so each stays a static string that
                // log aggregation can group on.
                match err {
                    shell::ShellError::Spawn(_) => tracing::error!(
                        task_id, stage = stage_name, %err,
                        "shell stage command could not be started"
                    ),
                    shell::ShellError::Io(_) => tracing::error!(
                        task_id, stage = stage_name, %err,
                        "shell stage command ran but its output could not be read"
                    ),
                }
                self.append_command_event(
                    task_id,
                    json!({
                        "stage": stage_name,
                        "command": described,
                        "exit_code": Value::Null,
                        "timed_out": false,
                        "duration_ms": elapsed_ms(started),
                        "stdout_tail": "",
                        "stderr_tail": "",
                        "note": err.to_string(),
                    }),
                )
                .await;
                self.finish_detached(
                    DetachedKind::Shell,
                    task_id,
                    definition,
                    stage_name,
                    "error",
                    None,
                )
                .await;
                return;
            }
        };

        let mut stage_outcome = if outcome.succeeded() { "done" } else { "error" };

        // Only a command that succeeded contributes a capture. A failed one
        // has nothing worth handing to a later stage, and storing it anyway
        // would be actively harmful: `stages.<name>` is keyed by stage, so
        // a stage re-entered by a retry loop would overwrite the good value
        // from the attempt that worked with the failed attempt's output (or,
        // on a timeout, with an empty string). A later
        // `{{ stages.open_pr.number }}` would then resolve against garbage.
        // The command's output is still on the timeline either way.
        let (captured, mut note) = if outcome.succeeded() && publishes_review {
            // A publisher's success is its answer: a command that exited 0
            // without printing a review reference did not publish one.
            let checked = if outcome.stdout_is_exact {
                validate_review_output(&outcome.stdout)
            } else {
                // `outcome.stdout` has U+FFFD where the command wrote bytes
                // that are not UTF-8; parsing it would accept text the
                // command never printed.
                Err("output is not valid UTF-8".to_string())
            };
            match checked {
                Ok(review) => (Some(review), None),
                Err(reason) => {
                    stage_outcome = "error";
                    (None, Some(format!("open_review output rejected: {reason}")))
                }
            }
        } else if outcome.succeeded() {
            derive_capture(capture, &outcome.stdout, task_id, stage_name, "stdout")
        } else if capture.is_some() {
            (
                None,
                Some("stdout not captured: the command did not succeed".to_string()),
            )
        } else {
            (None, None)
        };

        if outcome.timed_out {
            tracing::warn!(
                task_id,
                stage = stage_name,
                escaped = outcome.escaped,
                "shell stage command exceeded its timeout and was killed"
            );
        }
        // Outranks any capture note: the workflow is about to follow its
        // `on: error` edge, quite possibly straight back into this same
        // command, while the last one is still running in the same working
        // copy. The timeline is where an operator would find that out.
        //
        // Worded for what both `escaped` arms actually know — one saw the
        // group outlive SIGKILL, the other only failed to read its pipes —
        // and kept short enough to survive `choco task events`' 100-char
        // line budget, since being truncated before "may still be running"
        // would defeat the point of recording it.
        if outcome.escaped {
            note = Some(
                "could not confirm the process group exited — something may still be running"
                    .to_string(),
            );
        }

        let mut payload = json!({
            "stage": stage_name,
            "command": described,
            "exit_code": outcome.exit_code,
            "timed_out": outcome.timed_out,
            // A queryable sibling of `timed_out` rather than only a phrase
            // inside `note`, so "did any stage leave something running?" is
            // answerable from `choco --json` without matching free text.
            "escaped": outcome.escaped,
            "duration_ms": duration_ms(outcome.duration),
            "stdout_tail": tail(&outcome.stdout),
            "stderr_tail": tail(&outcome.stderr),
        });
        if let Some(note) = note {
            payload["note"] = Value::String(note);
        }
        self.append_command_event(task_id, payload).await;

        self.finish_detached(
            DetachedKind::Shell,
            task_id,
            definition,
            stage_name,
            stage_outcome,
            captured,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = "https://github.com/o/r/pull/7";
    const MERGE_REQUEST: &str = "https://git.example.internal/group/sub/proj/-/merge_requests/42";

    fn ok(stdout: &str) -> Value {
        validate_review_output(stdout).unwrap_or_else(|reason| panic!("{stdout:?}: {reason}"))
    }

    #[track_caller]
    fn rejected(stdout: &str, reason: &str) {
        match validate_review_output(stdout) {
            Err(got) => assert!(
                got.contains(reason),
                "{stdout:?}: wanted {reason:?} in {got:?}"
            ),
            Ok(value) => panic!("{stdout:?} was accepted as {value}"),
        }
    }

    /// A valid JSON object of exactly `len` bytes, padded in an extra field.
    fn document_of(len: usize) -> String {
        let build = |pad: usize| {
            format!(
                r#"{{"number":1,"url":"https://h/x/1","pad":"{}"}}"#,
                "p".repeat(pad)
            )
        };
        let document = build(len - build(0).len());
        assert_eq!(document.len(), len);
        document
    }

    #[test]
    fn accepted_output_is_kept_as_printed_with_a_label_filled_in() {
        // GitHub-shaped, label omitted: #<number>.
        assert_eq!(
            ok(&format!(r#"{{"number":7,"url":"{GITHUB}"}}"#)),
            json!({"number": 7, "url": GITHUB, "label": "#7"})
        );
        // A self-hosted merge request with its own label and extra fields.
        let printed = json!({"number": 42, "url": MERGE_REQUEST, "label": "!42",
                             "extra": "x", "nested": {"a": [1, null, 2.50]}});
        assert_eq!(ok(&format!("\n  {printed}  \n")), printed);
        // A supplied label is not trimmed or rewritten.
        assert_eq!(
            ok(&format!(
                r#"{{"number":1,"url":"{GITHUB}","label":"  !1  "}}"#
            ))["label"],
            "  !1  "
        );
        // Schemes match without regard to ASCII case.
        for url in ["HTTP://h/x", "HtTpS://h/x", "https://h", "http://h"] {
            ok(&format!(r#"{{"number":1,"url":"{url}"}}"#));
        }
        // The largest number a u64 holds.
        assert_eq!(
            ok(&format!(r#"{{"number":{},"url":"{GITHUB}"}}"#, u64::MAX))["number"],
            json!(u64::MAX)
        );
    }

    #[test]
    fn numbers_a_value_would_change_are_rejected_not_rounded() {
        for (printed, token) in [
            (r#""pos":18446744073709551617"#, "18446744073709551617"),
            (r#""neg":-9223372036854775809"#, "-9223372036854775809"),
            (
                r#""deep":{"a":[1,{"b":-12345678901234567890123.5}]}"#,
                "-12345678901234567890123.5",
            ),
            (
                r#""tiny":0.1000000000000000055511151231257827"#,
                "0.1000000000000000055511151231257827",
            ),
            (
                r#""list":[1,2,123456789012345678901234567890]"#,
                "123456789012345678901234567890",
            ),
        ] {
            rejected(
                &format!(r#"{{"number":7,"url":"https://h/7",{printed}}}"#),
                &format!("number {token} does not fit"),
            );
        }
        // `number` keeps its own rule's message.
        rejected(
            r#"{"number":18446744073709551617,"url":"https://h/7"}"#,
            "`number` must be a positive integer",
        );
    }

    #[test]
    fn numbers_a_value_keeps_and_text_that_only_looks_like_one_are_accepted() {
        let printed = format!(
            r#"{{"number":7,"url":"https://h/7","max":{},"min":{},"ratio":1.10,"exp":1e5,"neg":-2.5E-3,"zero":-0.0,"deep":{{"a":[0.1,{{"b":1e300}}]}},"text":"18446744073709551617 and \" 99999999999999999999","99999999999999999999":true,"flag":false}}"#,
            u64::MAX,
            i64::MIN
        );
        let capture = ok(&printed);
        assert_eq!(capture["max"], json!(u64::MAX));
        assert_eq!(capture["min"], json!(i64::MIN));
        assert_eq!(capture["ratio"], json!(1.1));
        assert_eq!(capture["exp"], json!(100000.0));
        assert_eq!(capture["deep"]["a"][1]["b"], json!(1e300));
        assert_eq!(
            capture["text"],
            "18446744073709551617 and \" 99999999999999999999"
        );
        assert_eq!(capture["99999999999999999999"], true);
    }

    #[test]
    fn an_object_shaped_like_serde_jsons_number_marker_stays_an_object() {
        let capture = ok(
            r#"{"number":7,"url":"https://h/7","extra":{"$serde_json::private::Number":"42"},"list":[{"$serde_json::private::Number":"1"}]}"#,
        );
        assert_eq!(
            capture["extra"],
            json!({"$serde_json::private::Number": "42"})
        );
        assert_eq!(
            capture["list"][0],
            json!({"$serde_json::private::Number": "1"})
        );
        // And in a required field it is a wrong type, not a number.
        rejected(
            r#"{"number":{"$serde_json::private::Number":"7"},"url":"https://h/7"}"#,
            "`number` must be a positive integer",
        );
    }

    #[test]
    fn the_size_limit_counts_trimmed_bytes_and_comes_first() {
        let at_limit = document_of(MAX_CAPTURE_BYTES);
        ok(&at_limit);
        // Surrounding whitespace is not counted.
        ok(&format!("\n\n  {at_limit}\t \n"));
        let over = document_of(MAX_CAPTURE_BYTES + 1);
        rejected(&over, "output exceeds the 1048576-byte capture limit");
        // Oversized and not JSON at all: the size is what is reported.
        rejected(
            &"x".repeat(MAX_CAPTURE_BYTES + 1),
            "output exceeds the 1048576-byte capture limit",
        );
        assert_eq!(
            validate_review_output(&over).unwrap_err(),
            "output exceeds the 1048576-byte capture limit"
        );
    }

    #[test]
    fn output_that_is_not_one_json_object_is_rejected() {
        let valid = format!(r#"{{"number":1,"url":"{GITHUB}"}}"#);
        rejected("", "output must be valid JSON");
        rejected("opened it", "output must be valid JSON");
        rejected(
            &format!("```json\n{valid}\n```"),
            "output must be valid JSON",
        );
        rejected(&format!("Opened: {valid}"), "output must be valid JSON");
        rejected(&format!("{valid}\n{valid}"), "output must be valid JSON");
        rejected(&format!("{valid} trailing"), "output must be valid JSON");
        for other in ["[1]", "null", "7", "\"text\"", "true"] {
            rejected(other, "output must be a JSON object");
        }
        rejected(&format!("[{valid}]"), "output must be a JSON object");
    }

    #[test]
    fn the_number_must_be_a_positive_integer() {
        // Zero first: dropping the positivity check fails here.
        for number in [
            "0",
            "-1",
            "7.0",
            r#""7""#,
            "null",
            "true",
            "18446744073709551616",
            "1e2",
        ] {
            rejected(
                &format!(r#"{{"number":{number},"url":"{GITHUB}"}}"#),
                "`number` must be a positive integer",
            );
        }
        rejected(
            &format!(r#"{{"url":"{GITHUB}"}}"#),
            "`number` must be a positive integer",
        );
    }

    #[test]
    fn the_url_must_be_an_http_url_without_whitespace() {
        rejected(r#"{"number":1}"#, "`url` must be a string");
        for url in ["null", "5", r#"["u"]"#] {
            rejected(
                &format!(r#"{{"number":1,"url":{url}}}"#),
                "`url` must be a string",
            );
        }
        let prefix = "`url` must start with http:// or https:// and have a nonempty remainder";
        for url in [
            "ftp://h/x",
            "h/x",
            "",
            "http://",
            "https://",
            "HTTPS://",
            "httpx://h",
        ] {
            rejected(&format!(r#"{{"number":1,"url":"{url}"}}"#), prefix);
        }
        // Every kind of whitespace, anywhere: ASCII, Unicode, in the middle or the end.
        for url in [
            "https://h/a b",
            "https://h/x ",
            "https://h/\\tx",
            "https://h/\\nx",
            "https://h/a\u{a0}b",
            "https://h/a\u{2003}b",
            "https://h/a\u{3000}",
        ] {
            rejected(
                &format!(r#"{{"number":1,"url":"{url}"}}"#),
                "`url` must contain no whitespace",
            );
        }
    }

    #[test]
    fn the_label_must_be_a_nonempty_single_line_string() {
        let reason = "`label` must be a nonempty single-line string";
        // The LF row comes before every other bad label: dropping the label
        // check fails here first.
        for label in [
            r#""a\nb""#,
            r#""""#,
            r#""   ""#,
            r#""a\rb""#,
            r#""a\r\nb""#,
            r#""\r""#,
            "null",
            "7",
            r#"["!1"]"#,
            "{}",
            "true",
        ] {
            rejected(
                &format!(r#"{{"number":1,"url":"{GITHUB}","label":{label}}}"#),
                reason,
            );
        }
    }
}
