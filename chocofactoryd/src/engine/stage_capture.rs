use super::turn::TURN_DEFAULT_OUTCOME;
use super::*;

/// Largest captured value stored in `workflow_state.payload`, *per stage* —
/// a workflow with several capturing stages can hold a multiple of this.
/// The payload is rewritten in full on every transition for the rest of the
/// task's life, so an unbounded capture from one chatty command would be
/// paid for again on every subsequent hop. Output past this cap isn't
/// captured at all — silently truncating it would hand a later stage a
/// value that looks whole but isn't.
pub(super) const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// Turns what a stage produced — a command's stdout, or an `agent_turn`'s
/// reply — into the value stored under `payload.stages.<stage>`, plus an
/// optional note for the timeline when something about that needed
/// explaining. `source` names that output in the note ("stdout", "the
/// reply"), since the same rules serve every kind that can capture.
///
/// A stage with no `capture:` stores nothing at all — only a stage that
/// asked for its output to be kept gets a payload entry.
///
/// Unparseable JSON under `capture: json` is deliberately *not* an error.
/// For `shell`/`poll`, §5.2 makes the exit code (or the `outcomes:` match)
/// the only thing that decides the outcome, so the output is kept as text
/// and the stage still reports what its exit code said. An `agent_turn`
/// follows the same rule rather than a stricter one of its own: capture is
/// one mechanism, and a turn whose reply carries no usable verdict falls
/// back to the outcome a plain turn emits (see `turn_outcome`). The note is
/// what tells a reader why the value isn't the object they expected.
pub(super) fn derive_capture(
    capture: Option<Capture>,
    output: &str,
    task_id: &str,
    stage_name: &str,
    source: &str,
) -> (Option<Value>, Option<String>) {
    let Some(capture) = capture else {
        return (None, None);
    };

    let trimmed = output.trim();
    if trimmed.len() > MAX_CAPTURE_BYTES {
        tracing::warn!(
            task_id,
            stage = stage_name,
            bytes = trimmed.len(),
            "stage output too large to capture; not stored"
        );
        return (
            None,
            Some(format!(
                "output not captured: {} bytes exceeds the {MAX_CAPTURE_BYTES}-byte limit",
                trimmed.len()
            )),
        );
    }

    match capture {
        Capture::Text => (Some(Value::String(trimmed.to_string())), None),
        Capture::Json => match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => (Some(value), None),
            Err(err) => {
                tracing::warn!(
                    task_id, stage = stage_name, %err,
                    "stage output was not valid JSON; captured as text"
                );
                (
                    Some(Value::String(trimmed.to_string())),
                    Some(format!(
                        "{source} was not valid JSON ({err}); captured as text"
                    )),
                )
            }
        },
    }
}

/// `derive_capture`, but for a `capture: json` agent turn's own reply only
/// (issue #73 review): additionally tries to recover the sole balanced
/// top-level `{…}` from a reply that isn't pure JSON — the fix for #73's
/// original repro, a reviewer that writes a sentence of preamble before its
/// verdict object.
///
/// Kept out of `derive_capture` itself, which a `shell`/`poll` stage's
/// stdout and a `human_gate`'s message also go through: those have no
/// comparable guarantee that a trailing `{…}` is the real verdict rather
/// than incidental JSON-shaped text the command printed or the human pasted,
/// so widening the recovery to them would trade a narrow, deterministic fix
/// for a much larger surface of "what looks like the answer."
pub(super) fn derive_agent_reply_capture(
    capture: Capture,
    reply: &str,
    task_id: &str,
    stage_name: &str,
) -> (Option<Value>, Option<String>) {
    if capture != Capture::Json {
        return derive_capture(Some(capture), reply, task_id, stage_name, "the reply");
    }

    let trimmed = reply.trim();
    if trimmed.len() > MAX_CAPTURE_BYTES {
        // Same oversized handling `derive_capture` gives every other
        // capturing stage kind.
        return derive_capture(Some(capture), reply, task_id, stage_name, "the reply");
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => (Some(value), None),
        Err(whole_err) => {
            match sole_top_level_json_object(trimmed).map(serde_json::from_str::<Value>) {
                Some(Ok(value)) => (
                    Some(value),
                    Some(format!(
                        "the reply was not valid JSON on its own ({whole_err}); recovered the sole \
                     '{{...}}' object in it"
                    )),
                ),
                _ => {
                    tracing::warn!(
                        task_id, stage = stage_name, %whole_err,
                        "stage output was not valid JSON; captured as text"
                    );
                    (
                        Some(Value::String(trimmed.to_string())),
                        Some(format!(
                            "the reply was not valid JSON ({whole_err}); captured as text"
                        )),
                    )
                }
            }
        }
    }
}

/// The outcome a completed `agent_turn` transitions on, and a note when that
/// wasn't what the stage's `capture:` implied it would be.
///
/// Under `capture: json` the reply's reserved `outcome` key is the verdict
/// (#45) — the whole point of capturing a turn as JSON. Anything else falls
/// back to `done`, the outcome §5.2 gives a plain single-shot turn.
///
/// That fallback is deliberate but not silent: it is recorded as a note on
/// the `turn_outcome` timeline entry and logged. The practical effect is that
/// a stage whose `on:` map declares real verdicts and no `done` edge — the
/// normal shape for a reviewer — parks for a human instead of guessing,
/// because `advance_from_stage` finds no `done` transition. A stage that
/// *does* declare `done` will take it, which is the accepted cost of one
/// lenient rule shared with `shell`/`poll` rather than two.
pub(super) fn turn_outcome(capture: Capture, captured: Option<&Value>) -> (String, Option<String>) {
    // `capture: text` keeps the reply but has no reserved key to read a
    // verdict out of. That isn't worth remarking on by itself — plenty of
    // stages capture text and route on `done` quite correctly — so the
    // explanation is added by `finish_turn`, and only when the stage
    // actually parked.
    if capture != Capture::Json {
        return (TURN_DEFAULT_OUTCOME.to_string(), None);
    }
    match captured.and_then(|value| value.get("outcome")) {
        // Trimmed for the same reason `outcome_from_report` trims (review,
        // #75 round 2): a reply's `on: {" approved ": ...}` would otherwise
        // match nothing `advance_from_stage` declares, parking a stage over
        // whitespace a human skimming the reply would never notice.
        Some(Value::String(outcome)) if !outcome.trim().is_empty() => {
            (outcome.trim().to_string(), None)
        }
        Some(Value::String(_)) => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the reply's 'outcome' was empty; advancing with '{TURN_DEFAULT_OUTCOME}'"
            )),
        ),
        Some(other) => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the reply's 'outcome' was {}, not a string; advancing with \
                 '{TURN_DEFAULT_OUTCOME}'",
                json_type_of(other)
            )),
        ),
        None => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the reply carried no 'outcome' key; advancing with '{TURN_DEFAULT_OUTCOME}'"
            )),
        ),
    }
}

/// Extracts `(outcome, note)` from a `report_outcome` tool call's `input`
/// (issue #73) — the same leniency `turn_outcome` applies to a `capture:
/// json` reply's `outcome` key, kept as a separate function because a
/// report's `input` isn't wrapped in a `Capture` the way a reply's parsed
/// JSON is.
///
/// Trims `outcome` before matching it against a stage's `on:` edges, for the
/// same reason `choco mcp-serve`'s own validation does (`call_tool`): the
/// `ToolCall` event this reads back records the model's argument verbatim,
/// untrimmed, and the tool already told the model its (trimmed) value was
/// accepted. Without the same trim here, `" approved "` would come back off
/// the timeline as a string `advance_from_stage` can't match against the
/// `approved` edge — parking a stage the tool just confirmed as routable.
///
/// `choco mcp-serve`'s own validation already rejects an off-list or empty
/// `outcome` with a tool error the model can act on and retry — but a model
/// that gave up after one rejection, rather than retrying, leaves that
/// off-list value as the last thing on the timeline. This function has no
/// stage-specific allow-list to check it against (only `advance_from_stage`
/// does), so an off-list value is passed through as-is here and left for
/// `advance_from_stage` to reject as an `UnknownOutcome` — the same park a
/// bad reply-JSON `outcome` produces today, not a silent `done`. Only a
/// missing, non-string, or (after trimming) empty `outcome` falls back to
/// `done`, mirroring `turn_outcome`'s leniency for the same shapes.
pub(super) fn outcome_from_report(report: &Value) -> (String, Option<String>) {
    match report.get("outcome") {
        Some(Value::String(outcome)) if !outcome.trim().is_empty() => {
            (outcome.trim().to_string(), None)
        }
        Some(Value::String(_)) => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the report's 'outcome' was empty; advancing with '{TURN_DEFAULT_OUTCOME}'"
            )),
        ),
        Some(other) => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the report's 'outcome' was {}, not a string; advancing with \
                 '{TURN_DEFAULT_OUTCOME}'",
                json_type_of(other)
            )),
        ),
        None => (
            TURN_DEFAULT_OUTCOME.to_string(),
            Some(format!(
                "the report carried no 'outcome' key; advancing with '{TURN_DEFAULT_OUTCOME}'"
            )),
        ),
    }
}

/// Strips a surrounding ```` ``` ```` fence from a turn's reply.
///
/// The one normalization applied to a reply before it is captured, and it
/// earns its place: wrapping structured output in a fenced block is the single
/// commonest thing a model does unbidden, and without this a `capture: json`
/// reviewer would fail to parse, fall back to `done`, and route the graph on a
/// verdict it never gave. Everything else is left exactly as the agent wrote
/// it — this is not a general "find the JSON somewhere in the prose" search,
/// which would be guessing.
///
/// Only an *entire* reply that is one fenced block is unwrapped; a fence in
/// the middle of prose is left alone, since that reply wasn't a document.
///
/// Applied for `capture: text` too, not just `json`. A text capture of a
/// reply the agent chose to fence almost certainly wants the contents rather
/// than the markup, and one rule for both beats a mode-dependent surprise.
pub(super) fn unwrap_code_fence(reply: &str) -> &str {
    let Some(rest) = reply.strip_prefix("```") else {
        return reply;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return reply;
    };
    // Drop the info string (` ```json `), which is the rest of the opening
    // line. A fence with no newline at all isn't a block.
    let Some((_info, body)) = body.split_once('\n') else {
        return reply;
    };
    // A second fence inside means this was prose containing two blocks, not
    // one document.
    if body.contains("```") {
        return reply;
    }
    body.trim()
}

/// The one top-level `{...}` span in `text` that parses as valid JSON, or
/// `None` if there is no such span, *or more than one* (issue #73; narrowed
/// further on review, #75 round 2).
///
/// The fallback `derive_agent_reply_capture` reaches for when the whole
/// reply doesn't parse as JSON — prose before or after an otherwise
/// well-formed object, the shape a reviewer's non-compliant reply took in
/// #73's original report. Narrow and deterministic, the same kind of
/// concession `unwrap_code_fence` already makes for a reply wrapped in a
/// fence: this looks only for braces that open *outside* any other object,
/// never inside one.
///
/// Unlike the rest of this function's scan, validating each candidate as
/// JSON (rather than just checking its braces balance) happens here, not in
/// the caller: a brace-balanced span that *isn't* valid JSON — `{a}` in "the
/// diff touches {a}, then {"outcome": "approved"}" — is prose that merely
/// looks like an object, not a competing candidate, and must not make an
/// otherwise-unambiguous verdict un-recoverable.
///
/// Requiring exactly one *valid* candidate, rather than taking the last of
/// however many are found, is what actually closes the gap a plain "last
/// object wins" rule leaves open: a reviewer that illustrates an example
/// verdict before stating its real one (`"a rejected reply looks like
/// {"outcome": ...}. My verdict: {"outcome": "approved", ...}"`) produces
/// two equally well-formed objects, and nothing about their shape says
/// which one is real. Two or more valid candidates means the reply is
/// ambiguous, not that the later one wins; `derive_agent_reply_capture`
/// falls through to capturing the reply as plain text in that case, same as
/// finding none at all.
///
/// Tracks string literals and backslash escapes while scanning so a `{` or
/// `}` inside a JSON string value (a reviewer's own feedback text, say)
/// can't unbalance the depth count. Byte-indexed rather than char-indexed:
/// safe because every delimiter this function looks for (`{`, `}`, `"`, `\`)
/// is single-byte ASCII, and no UTF-8 continuation byte can equal one of
/// them, so every slice boundary this produces still lands on a char
/// boundary.
pub(super) fn sole_top_level_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = None;
    let mut valid: Option<(usize, usize)> = None;
    let mut ambiguous = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start
                    && serde_json::from_str::<Value>(&text[s..i + 1]).is_ok()
                {
                    if valid.is_some() {
                        ambiguous = true;
                    }
                    valid = Some((s, i + 1));
                }
            }
            _ => {}
        }
    }

    if ambiguous {
        return None;
    }
    valid.map(|(s, e)| &text[s..e])
}

fn json_type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// How a stage's `capture:` reads on the timeline.
pub(super) fn capture_label(capture: Option<Capture>) -> Value {
    match capture {
        Some(Capture::Json) => Value::String("json".to_string()),
        Some(Capture::Text) => Value::String("text".to_string()),
        None => Value::Null,
    }
}

/// Stores a stage's captured stdout at `payload.stages.<stage>` (§5.1).
///
/// The `stages` namespace is explicit rather than the payload root because
/// `payload` is one shared blob for the whole task — reserving a top-level
/// key leaves room for other engine-owned namespaces later without a
/// migration, and without a workflow whose stage is *named* `stages`
/// colliding with one. The path matches the `{{ stages.<name>.<field> }}`
/// templating P2-3 will resolve against it.
///
/// Re-entering a stage overwrites its previous capture: the value means
/// "what this stage produced most recently", which is what a later stage
/// templating it wants. The stage trail on the events timeline is what
/// records that it ran more than once.
pub(super) fn merge_stage_capture(payload: &mut Value, stage: &str, value: Value) {
    // A payload that isn't an object (hand-edited row, or a shape some
    // future writer chose) would silently swallow the capture if this
    // just gave up, so replace it — the engine owns this column, and
    // nothing else writes it today.
    if !payload.is_object() {
        *payload = json!({});
    }
    let stages = payload
        .as_object_mut()
        .expect("payload was just ensured to be an object")
        .entry("stages")
        .or_insert_with(|| json!({}));
    if !stages.is_object() {
        *stages = json!({});
    }
    stages
        .as_object_mut()
        .expect("stages was just ensured to be an object")
        .insert(stage.to_string(), value);
}
