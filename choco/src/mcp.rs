//! `choco mcp-serve` (issue #73): a one-tool MCP server the daemon wires into
//! every agent turn so an agent can *state* its outcome instead of leaving the
//! engine to infer one from prose.
//!
//! The problem it replaces: a `capture: json` stage used to recognise a
//! verdict only when the agent's entire reply parsed as JSON, so a sentence of
//! preamble discarded a perfectly good verdict and parked the task. A tool
//! call is unambiguous, and — unlike a reply — a bad argument can be rejected
//! with an error the model can act on and retry.
//!
//! Two deliberate limits:
//!
//! - **No daemon.** It never contacts `chocofactoryd`. The daemon already
//!   records every tool call an agent makes as a `tool_call` event, so it
//!   reads the verdict back off that timeline exactly as it already reads the
//!   final assistant text. That leaves nothing here to authenticate and no
//!   endpoint to add. The one thing it does keep (issue #95) is a count of
//!   how many reports it has turned away for missing sections, which lives
//!   in this process — one server process per CLI session, one thread
//!   reading one stdio stream, so there is nothing for it to race with.
//!   That count is shared with anything else calling the tool over the
//!   same connection, a sub-agent included; the server can't tell callers
//!   apart, and the direction it fails in is lenient (an allowance spent
//!   early, never a turn parked), so it is left as is.
//! - **Hand-rolled.** Four JSON-RPC methods over newline-delimited stdio, no
//!   MCP SDK. `serde_json` is already a dependency; a crate for one tool
//!   would not be.
//!
//! The allowed `outcome` values are passed in with repeated `--outcome`
//! flags and come from the stage's `on:` map, and the sections a report must
//! carry come from the stage's `report_sections:` with repeated
//! `--require-section` flags, so this binary has no idea what a "reviewer"
//! is — every agent turn gets the same tool, and only the lists differ.

use std::io::{BufRead, Write};

use chocofactory_core::mcp::{MCP_SERVER_NAME, REPORT_OUTCOME_TOOL_NAME, normalize_report_heading};
use serde_json::{Value, json};

/// The MCP protocol version answered with when a client doesn't name one.
///
/// `initialize` normally echoes the client's requested version back: this
/// server implements nothing version-specific, so agreeing with whatever the
/// caller speaks is both honest and maximally compatible.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// The tool's unqualified name. Agents see it namespaced by the server key the
/// daemon writes into `--mcp-config`, i.e. `mcp__chocofactory__report_outcome`
/// — both halves come from `chocofactory_core::mcp` so this and the daemon's
/// own lookup can never disagree about what the tool is called.
pub const TOOL_NAME: &str = REPORT_OUTCOME_TOOL_NAME;

/// How many reports this server turns away for missing sections before it
/// takes one anyway (issue #95).
///
/// The rejection is the point — a reviewer that stopped at its first
/// blocking finding has no "Branches → tests" walk to write down, and being
/// sent back is what makes it do the walk. But a turn that *never* reports
/// is worse than a thin one: the session nudges it, runs out of nudges and
/// parks the task for a human (#73, #91). So a model that can't satisfy the
/// rule costs two retries, and its third call is recorded with the gap named
/// in the tool's reply — which lands on the task's timeline as a
/// `tool_result` event, where whoever reads the review can see it.
const MAX_SECTION_REJECTIONS: u32 = 2;

/// What the stage asks of a report: which `outcome` values route it
/// (`--outcome`), and which sections its `summary` must carry
/// (`--require-section`).
///
/// One struct rather than two parameters threaded side by side, because
/// every layer between the stage definition and this server passes both
/// together and neither means anything without the stage it came from.
#[derive(Debug, Default, Clone)]
pub struct StageReport {
    pub outcomes: Vec<String>,
    pub required_sections: Vec<String>,
}

/// Serves the tool over `input`/`output` until the client closes the stream.
///
/// Split from the subcommand entry point so tests can drive a whole session
/// over byte slices, the same reason `client`'s request builders are split
/// from their sending halves.
pub fn serve(
    stage: &StageReport,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    // Per session, not per call: see `MAX_SECTION_REJECTIONS`.
    let mut thin_reports = 0;
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(stage, &mut thin_reports, &line) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Answers one request line. `None` means "say nothing", which is required
/// rather than merely polite: a JSON-RPC *notification* has no `id`, and
/// replying to one is a protocol violation.
fn handle_line(stage: &StageReport, thin_reports: &mut u32, line: &str) -> Option<String> {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        // No `id` is recoverable from an unparseable line, so this is the one
        // place a null id is correct rather than sloppy.
        Err(err) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ));
        }
    };

    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = request.get("id").cloned();

    // Notifications (`notifications/initialized` and any other) carry no `id`
    // and get no response at all.
    let id = id?;

    let result = match method {
        "initialize" => Ok(initialize_result(&request)),
        "tools/list" => Ok(json!({ "tools": [tool_definition(stage)] })),
        "tools/call" => match call_tool(stage, thin_reports, request.get("params")) {
            Ok(result) => Ok(result),
            Err(CallError::Protocol(message)) => Err((-32602, message)),
        },
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("unknown method '{other}'"))),
    };

    Some(match result {
        Ok(result) => success_response(id, result),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn initialize_result(request: &Value) -> Value {
    let protocol_version = request
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
    })
}

/// A failure that belongs in the JSON-RPC `error` field because the *client*
/// is malformed.
///
/// A bad *argument* is deliberately not one of these — see `call_tool`.
enum CallError {
    Protocol(String),
}

/// The `tools/call` handler.
///
/// The distinction that makes this worth having at all: a malformed request
/// is a JSON-RPC error, but a *rejected argument* comes back as a normal
/// result with `isError: true`. Only the second form reaches the model, so
/// only the second form gives it the chance to correct itself and call again
/// — which is precisely what the old "reply with nothing but this object"
/// instruction could never offer.
fn call_tool(
    stage: &StageReport,
    thin_reports: &mut u32,
    params: Option<&Value>,
) -> Result<Value, CallError> {
    let outcomes = &stage.outcomes[..];
    let params = params.ok_or_else(|| CallError::Protocol("missing params".to_string()))?;

    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if name != TOOL_NAME {
        return Err(CallError::Protocol(format!("unknown tool '{name}'")));
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome = match arguments.get("outcome").and_then(Value::as_str) {
        Some(outcome) if !outcome.trim().is_empty() => outcome.trim(),
        _ => {
            return Ok(tool_error(&format!(
                "'outcome' is required and must be a non-empty string.{}",
                allowed_clause(outcomes)
            )));
        }
    };

    if !outcomes.is_empty() && !outcomes.iter().any(|allowed| allowed == outcome) {
        return Ok(tool_error(&format!(
            "'{outcome}' is not a valid outcome for this stage.{} Call this tool again with one \
             of those.",
            allowed_clause(outcomes)
        )));
    }

    // Review, #75: `tool_definition` documents `summary` as required (may be
    // empty, but always present, so a template reading it never renders
    // empty because the field is simply missing) — but nothing enforced
    // that until now. The MCP `inputSchema`'s own `"required"` array is a
    // hint to the model, not something this server validates on its
    // behalf; skip this check and the doc comment's promise is false.
    let Some(summary) = arguments.get("summary").and_then(Value::as_str) else {
        return Ok(tool_error(
            "'summary' is required and must be a string (it may be empty).",
        ));
    };

    // Issue #95: the stage can require the report to carry named sections,
    // which is what turns "I have enough to reject" into "I walked every
    // branch, state and message". Checked after the outcome so a call that
    // is wrong in both ways is told about the cheaper problem first, and
    // doesn't spend a retry learning about the second one only afterwards.
    let missing = missing_sections(summary, &stage.required_sections);
    if !missing.is_empty() {
        // Counted before the branch, and never saturated, so the number in
        // the message below is the attempt this actually is (review of
        // #95) rather than a fixed "3" on every later call.
        *thin_reports += 1;
        if *thin_reports <= MAX_SECTION_REJECTIONS {
            return Ok(tool_error(&missing_sections_message(
                &missing,
                &stage.required_sections,
            )));
        }
        // Taken anyway, with the gap stated in the reply rather than
        // silently dropped: see `MAX_SECTION_REJECTIONS`.
        return Ok(tool_success(&format!(
            "Recorded outcome '{outcome}'. Its report is still missing {}, and this is attempt \
             {}, so it was recorded as it stands.",
            quoted_list(&missing),
            *thin_reports,
        )));
    }

    Ok(tool_success(&format!("Recorded outcome '{outcome}'.")))
}

/// The required sections `summary` doesn't account for, in the order the
/// stage declared them. A section is accounted for when the summary has a
/// heading line for it *and* something under it.
///
/// Empty when `required` is empty, which is every stage that doesn't ask
/// for sections — i.e. all of them before #95, and every workflow that
/// never opts in.
fn missing_sections(summary: &str, required: &[String]) -> Vec<String> {
    if required.is_empty() {
        return Vec::new();
    }
    let normalized_names: Vec<String> = required
        .iter()
        .map(|name| normalize_report_heading(name))
        .collect();
    let lines: Vec<&str> = summary.lines().collect();
    // Which required section each line is a heading for, so the loop below
    // can tell "the next heading" (which ends a section's content) from an
    // ordinary line without matching twice.
    let headings: Vec<Option<(usize, String)>> = lines
        .iter()
        .map(|line| heading_for(line, &normalized_names))
        .collect();

    let mut satisfied = vec![false; required.len()];
    for (index, heading) in headings.iter().enumerate() {
        let Some((section, rest_of_line)) = heading else {
            continue;
        };
        if satisfied[*section] {
            continue;
        }
        // Content is whatever follows the heading on its own line, plus
        // every line after it up to the next heading. A report that writes
        // "Findings: none" on one line and one that writes "Findings" with
        // "none" beneath it both count.
        //
        // "Next heading" means a heading for a section not yet accounted
        // for (review of #95, round 3). A terse line inside a section —
        // "Old behaviour preserved" under Dismissed — reads as a heading
        // for a walk written earlier in the report, and treating it as a
        // boundary left Dismissed looking empty: the report was rejected
        // for a section plainly there with content under it, which is the
        // one failure this rule must not produce.
        //
        // A repeat of *this* section's own heading ends the scan like any
        // other heading (rounds 4 and 5): a heading line is never blank,
        // so a scan that ran past it counted it as content, and `## States`
        // written twice with nothing under either satisfied States — a
        // cheaper way to fake a walk than the empty heading this whole
        // check exists to catch. Walking past it and then declining to
        // count it fixed that but cost `any`'s short-circuit, making the
        // pass quadratic on a document of repeated headings (22s on 50k
        // of them, against 0.15s before). Ending the scan is both.
        //
        // Accepted residual: an empty section followed by a repeat of a
        // *different*, already-satisfied heading is still credited. At
        // this level that is the same line as the terse-line case above —
        // "Old behaviour preserved" is both a heading for an earlier walk
        // and legitimate Dismissed content — and rejecting it would put
        // back the false rejection that is worse.
        let has_content = !rest_of_line.trim().is_empty()
            || lines[index + 1..]
                .iter()
                .zip(&headings[index + 1..])
                .take_while(|(_, heading)| match heading {
                    Some((other, _)) => satisfied[*other],
                    None => true,
                })
                .any(|(line, _)| !line.trim().is_empty());
        satisfied[*section] = has_content;
    }

    required
        .iter()
        .zip(satisfied)
        .filter(|(_, satisfied)| !satisfied)
        .map(|(name, _)| name.clone())
        .collect()
}

/// `(index into normalized_names, the rest of the line after the name)` when
/// `line` is a heading for one of them.
///
/// Prefix rather than equality, because a heading is rarely bare: reports
/// write `## Findings`, `**Findings**`, `Findings (defects):` and
/// `Findings: none` and all four mean the same thing.
///
/// What may follow the name is the whole difficulty, and #95's own review
/// found it the hard way. A line is only a heading when what follows the
/// name is punctuation or nothing:
///
/// - `Findings: none`, `Findings (defects):`, `Findings —` are headings.
/// - `Side effects of the retry are untested` is a *finding that begins
///   with a section's name*, not the "Side effects" heading. Reading it as
///   one both satisfied a section nobody wrote and cut the enclosing
///   section's content short, so a report with the section plainly there
///   was rejected as missing it.
/// - Seven bullets under "Prior findings", each naming a section
///   ("- Findings F1 resolved at engine.rs:329"), are findings about those
///   walks, not the walks — the same rule catches them, because what
///   follows the name is prose.
///
/// A bullet is *not* held to a stricter rule than that. Round 2 of #95's
/// own review tried it, and `- **Side effects:** one INSERT` — the format
/// `reviewer-system.md` itself lists the walks in — then matched nothing
/// at all, so a reviewer was told all ten sections were missing while
/// looking at all ten it had written.
///
/// The character after the name must also not be alphanumeric, so
/// `Findings` doesn't match a sentence starting "Findingsomething", and
/// `Prior findings` — a section only a re-review has — doesn't satisfy a
/// required `Findings`.
fn heading_for(line: &str, normalized_names: &[String]) -> Option<(usize, String)> {
    let normalized_line = normalize_report_heading(line);
    let (index, name) = normalized_names
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            normalized_line
                .strip_prefix(name.as_str())
                .is_some_and(heads_a_section)
        })
        // Longest match wins, so a stage that requires both `Findings` and
        // `Findings (blocking)` can't have the shorter one swallow the
        // longer one's heading.
        .max_by_key(|(_, name)| name.len())?;

    // The rest is taken from the *normalized* line, sliced at the
    // normalized name's length, because normalizing rewrites `→` into `->`
    // and so shifts byte offsets away from the raw line's. It is only ever
    // tested for emptiness, never shown, so losing the original casing and
    // spacing costs nothing. Punctuation a heading ends with (`Findings:`,
    // `Findings —`) is not content.
    let rest = normalized_line[name.len()..]
        .trim_start_matches([':', '-', '—', '–', '*', '_', '#', '.', ' ', '\t'])
        .to_string();
    Some((index, rest))
}

/// Whether `rest` — what a line has left after a section's name — leaves
/// the line reading as that section's heading.
///
/// Two shapes count: nothing at all (`## Findings`) and punctuation
/// (`Findings:`, `Findings (defects)`, `Side effects — one INSERT`).
/// Anything else is a sentence that happens to open with a section's
/// name, and those are findings, not headings.
///
/// A single bare word was allowed for a while, so that `Reviewed 30161d9`
/// — the commit line the prompt asks for — counted. It was withdrawn:
/// `- Findings resolved.` under "Prior findings" is the same shape, and
/// on a re-review lap that credited the findings walk to a line about the
/// *previous* lap's findings. The prompt asks for `Reviewed: <sha>`
/// instead, and a report that writes the bare form is told once that
/// Reviewed is missing, which it can fix on the retry.
fn heads_a_section(rest: &str) -> bool {
    // `Findings` must not be credited by a line reading
    // `Findingsomething`: a letter straight after the name means the name
    // is only the start of a longer word.
    if rest.starts_with(char::is_alphanumeric) {
        return false;
    }
    let rest = rest.trim();
    rest.is_empty()
        || rest.starts_with([':', '(', '[', '{', '—', '–', '-', ',', '.', '/', '*', '#'])
}

/// The tool error a report with missing sections comes back with.
///
/// Names what's missing *and* the full list in order, because a model that
/// left out one section usually can't tell which of its headings the server
/// matched — and a second rejection for a different section would cost
/// another lap.
fn missing_sections_message(missing: &[String], required: &[String]) -> String {
    format!(
        "Your report's 'summary' is missing {}: each required section needs a heading line with \
         something under it (write 'none' when there is genuinely nothing). Required sections, in \
         order: {}. Finish the walks you skipped, then call this tool again with the complete \
         report.",
        quoted_list(missing),
        required.join(", "),
    )
}

/// `'a'`, `'a' and 'b'`, `'a', 'b' and 'c'`.
fn quoted_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("'{item}'")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

fn tool_success(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

/// ` Allowed values are: a, b.` — or nothing at all when the stage declares no
/// outcomes, where naming an empty list would be worse than saying nothing.
fn allowed_clause(outcomes: &[String]) -> String {
    if outcomes.is_empty() {
        String::new()
    } else {
        format!(" Allowed values are: {}.", outcomes.join(", "))
    }
}

/// ` Its 'summary' must carry these sections, in order: a, b.` — or nothing
/// when the stage requires none, which is every stage that hasn't opted in.
fn sections_clause(required_sections: &[String]) -> String {
    if required_sections.is_empty() {
        String::new()
    } else {
        format!(
            " Its 'summary' must carry these sections, best written in this order: {}.",
            required_sections.join(", ")
        )
    }
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// The tool as the model sees it.
///
/// Both the description and the schema are generated from `outcomes`, which is
/// the whole reason no workflow author ever writes about this tool in a prompt
/// file: there is no second copy of the stage's `on:` keys to fall out of sync.
fn tool_definition(stage: &StageReport) -> Value {
    let outcomes = &stage.outcomes[..];
    let mut outcome_schema = json!({
        "type": "string",
        "description": if outcomes.is_empty() {
            "A short outcome label for this stage.".to_string()
        } else {
            format!("One of: {}.", outcomes.join(", "))
        },
    });

    let description = if outcomes.is_empty() {
        // Said plainly so an agent doesn't spend a call reporting into the
        // void: this stage's transition is fixed, and the report is only ever
        // read by a human skimming the timeline.
        format!(
            "Report a status for this stage. Optional — this stage does not route on reported \
             outcomes, so the report is recorded on the task's timeline but does not affect what \
             happens next.{}",
            sections_clause(&stage.required_sections),
        )
    } else {
        outcome_schema["enum"] = json!(outcomes);
        format!(
            "Report this stage's outcome so the workflow can route on it. Call this before you \
             end your turn. 'outcome' must be exactly one of: {}.{}",
            outcomes.join(", "),
            sections_clause(&stage.required_sections),
        )
    };

    // #95: what `summary` must carry, generated from the stage's
    // `report_sections:` for the same reason the outcome `enum` is
    // generated from its `on:` keys — a second copy in a prompt file is a
    // second thing to fall out of sync with what the server enforces.
    //
    // Writing the summary first is asked for in words rather than by
    // ordering the schema's properties: `serde_json`'s object keys are
    // sorted, so `outcome` precedes `summary` there whatever this code
    // does.
    let summary_description = format!(
        "Your reasoning, specific enough for whoever acts on this next to work from without \
         re-reading your whole turn. Write this out in full *before* you settle on 'outcome': \
         the outcome follows from what you found, not the other way round.{}",
        if stage.required_sections.is_empty() {
            " May be empty when there is nothing to add.".to_string()
        } else {
            format!(
                " This stage requires these sections, each with a heading line and something \
                 under it (write 'none' when there is genuinely nothing): {}. Write them in that \
                 order. A report missing any of them is rejected and you will be asked to call \
                 again.",
                stage.required_sections.join(", "),
            )
        },
    );

    json!({
        "name": TOOL_NAME,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "outcome": outcome_schema,
                "summary": { "type": "string", "description": summary_description },
            },
            // `summary` is required-but-may-be-empty rather than optional, so
            // the captured object always carries the key and a workflow
            // templating `{{ stages.<stage>.summary }}` can't silently render
            // empty because the agent omitted it.
            "required": ["outcome", "summary"],
        },
    })
}

fn success_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i32, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcomes() -> StageReport {
        StageReport {
            outcomes: vec!["approved".to_string(), "changes_requested".to_string()],
            required_sections: Vec::new(),
        }
    }

    /// The same stage, plus the sections #95 has `internal_review` require.
    fn with_sections(sections: &[&str]) -> StageReport {
        StageReport {
            required_sections: sections.iter().map(|s| s.to_string()).collect(),
            ..outcomes()
        }
    }

    /// A stage with no `on:` edges — chat's shape.
    fn no_outcomes() -> StageReport {
        StageReport::default()
    }

    /// Drives a whole session and returns one parsed response per line the
    /// server wrote — so a test can assert on what a client would actually
    /// see, including *how many* responses it got.
    fn session(stage: &StageReport, requests: &[Value]) -> Vec<Value> {
        let input = requests
            .iter()
            .map(|request| request.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = Vec::new();
        serve(stage, input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn call(stage: &StageReport, arguments: Value) -> Value {
        let responses = session(
            stage,
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": TOOL_NAME, "arguments": arguments },
            })],
        );
        responses[0]["result"].clone()
    }

    #[test]
    fn initialize_echoes_the_client_protocol_version() {
        let responses = session(
            &outcomes(),
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05" },
            })],
        );
        assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");
        assert!(responses[0]["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_when_the_client_names_no_version() {
        let responses = session(
            &outcomes(),
            &[json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })],
        );
        assert_eq!(
            responses[0]["result"]["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    /// Replying to a notification is a protocol violation, and the one the
    /// real client sends immediately after `initialize`.
    #[test]
    fn notifications_get_no_response() {
        let responses = session(
            &outcomes(),
            &[
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }),
            ],
        );
        assert_eq!(responses.len(), 1, "got {responses:?}");
        assert_eq!(responses[0]["id"], 7);
    }

    #[test]
    fn tools_list_constrains_outcome_to_the_stages_edges() {
        let responses = session(
            &outcomes(),
            &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
        );
        let tool = &responses[0]["result"]["tools"][0];
        assert_eq!(tool["name"], TOOL_NAME);
        assert_eq!(
            tool["inputSchema"]["properties"]["outcome"]["enum"],
            json!(["approved", "changes_requested"])
        );
        assert_eq!(
            tool["inputSchema"]["required"],
            json!(["outcome", "summary"])
        );
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("approved"), "got {description}");
        assert!(
            description.contains("changes_requested"),
            "got {description}"
        );
    }

    /// A stage with no `on:` edges declares no outcomes, and the description
    /// has to say so — that, not a line in `coder-system.md`, is what stops a
    /// coder reporting into the void.
    #[test]
    fn tools_list_leaves_outcome_free_form_when_the_stage_declares_none() {
        let responses = session(
            &no_outcomes(),
            &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
        );
        let tool = &responses[0]["result"]["tools"][0];
        assert!(tool["inputSchema"]["properties"]["outcome"]["enum"].is_null());
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("does not route"), "got {description}");
    }

    #[test]
    fn a_valid_outcome_is_accepted() {
        let result = call(
            &outcomes(),
            json!({ "outcome": "approved", "summary": "looks right" }),
        );
        assert_eq!(result["isError"], false, "got {result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("approved")
        );
    }

    /// The point of the whole design: a wrong argument comes back as a
    /// *result*, not a JSON-RPC error, because only a result reaches the model
    /// and lets it try again.
    #[test]
    fn an_off_list_outcome_is_a_tool_error_naming_the_allowed_values() {
        let responses = session(
            &outcomes(),
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": TOOL_NAME,
                    "arguments": { "outcome": "lgtm", "summary": "" },
                },
            })],
        );
        assert!(responses[0]["error"].is_null(), "got {:?}", responses[0]);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], true, "got {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("approved"), "got {text}");
        assert!(text.contains("changes_requested"), "got {text}");
    }

    #[test]
    fn a_missing_outcome_is_a_tool_error() {
        let result = call(&outcomes(), json!({ "summary": "forgot the verdict" }));
        assert_eq!(result["isError"], true, "got {result}");
    }

    #[test]
    fn an_empty_outcome_is_a_tool_error() {
        let result = call(&outcomes(), json!({ "outcome": "   ", "summary": "" }));
        assert_eq!(result["isError"], true, "got {result}");
    }

    /// Review, #75: `tool_definition` documents `summary` as always present
    /// on a successful call (required, though it may be empty) — this pins
    /// that the server actually enforces it, not just that its schema hints
    /// it. Without this, a model calling `report_outcome({"outcome": "x"})`
    /// with no `summary` at all succeeded, and the daemon would later merge
    /// a captured object with no `summary` key — reproducing #73's original
    /// symptom (`coder-revise.md`'s `{{ stages.internal_review.summary }}`
    /// rendering empty) through the *primary* tool path, not just the
    /// text-fallback one.
    #[test]
    fn a_missing_summary_is_a_tool_error() {
        let result = call(&outcomes(), json!({ "outcome": "approved" }));
        assert_eq!(result["isError"], true, "got {result}");
    }

    /// `summary` may be empty — only its *presence* (and type) is required.
    #[test]
    fn an_empty_summary_is_accepted() {
        let result = call(&outcomes(), json!({ "outcome": "approved", "summary": "" }));
        assert_eq!(result["isError"], false, "got {result}");
    }

    #[test]
    fn a_non_string_summary_is_a_tool_error() {
        let result = call(&outcomes(), json!({ "outcome": "approved", "summary": 7 }));
        assert_eq!(result["isError"], true, "got {result}");
    }

    /// #95's whole point: the verdict alone isn't a report. A reviewer that
    /// stopped at its first blocking finding has nothing to write under the
    /// walks, and is sent back with the list.
    #[test]
    fn a_report_missing_a_required_section_is_a_tool_error_naming_it() {
        let stage = with_sections(&["Findings", "Branches → tests"]);
        let result = call(
            &stage,
            json!({
                "outcome": "changes_requested",
                "summary": "Findings\n- F1: the allowlist is untested.\n",
            }),
        );
        assert_eq!(result["isError"], true, "got {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("'Branches → tests'"), "got {text}");
        assert!(!text.contains("'Findings'"), "got {text}");
    }

    /// A heading with nothing under it is the cheapest way to look like you
    /// did the walk, so it counts as missing rather than present.
    #[test]
    fn a_required_section_with_no_content_is_missing() {
        let result = call(
            &with_sections(&["States", "Findings"]),
            json!({
                "outcome": "approved",
                "summary": "## States\n\n## Findings\nnone blocking\n",
            }),
        );
        assert_eq!(result["isError"], true, "got {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("'States'"), "got {text}");
    }

    /// Round 4 of #95's review: the heading line the scan walks past is
    /// never blank, so a section heading written twice with nothing under
    /// either satisfied it — a cheaper way to fake a walk than the single
    /// empty heading `a_required_section_with_no_content_is_missing`
    /// pins, and invisible to that test because it writes one heading.
    #[test]
    fn a_section_heading_repeated_with_no_content_is_still_missing() {
        for summary in ["## States\n\n## States\n", &"## States\n".repeat(200)] {
            let result = call(
                &with_sections(&["States"]),
                json!({ "outcome": "approved", "summary": summary }),
            );
            assert_eq!(result["isError"], true, "summary {summary:?} gave {result}");
        }
    }

    /// The scan must stay linear. An earlier fix for the doubled-heading
    /// case walked past a repeat of the section's own heading and then
    /// declined to count it, which removed `any`'s short-circuit and made
    /// a document of repeated headings quadratic — 50,000 of them went
    /// from 0.15s to 22s, and a summary is allowed to be a megabyte.
    #[test]
    fn a_document_of_repeated_headings_stays_linear() {
        let summary = "## States\n".repeat(20_000);
        let started = std::time::Instant::now();
        let missing = missing_sections(&summary, &["States".to_string()]);
        assert_eq!(missing, vec!["States".to_string()]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "20k repeated headings took {:?}; the scan has gone quadratic again",
            started.elapsed()
        );
    }

    /// Reports in the wild write headings every which way — `## Findings`,
    /// `**Side effects**`, `Messages:` with the content on the same line,
    /// `Branches -> tests` with an ASCII arrow. All of them did the walk.
    #[test]
    fn headings_are_matched_through_markdown_case_and_arrow_spelling() {
        let result = call(
            &with_sections(&["Branches → tests", "Side effects", "Messages", "Findings"]),
            json!({
                "outcome": "approved",
                "summary": "## branches -> TESTS\n- resolve_task_workflow: covered\n\n\
                            **Side effects**\nOne INSERT, recorded once.\n\n\
                            Messages: the NotFound text is accurate.\n\n\
                            - Findings (none blocking)\n  nothing to report\n",
            }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// "Prior findings" is a re-review's own section and must not be read as
    /// the "Findings" every report owes, or a lap that resolved everything
    /// could skip the findings walk entirely.
    #[test]
    fn a_longer_heading_does_not_satisfy_a_different_required_section() {
        let result = call(
            &with_sections(&["Findings"]),
            json!({
                "outcome": "approved",
                "summary": "Prior findings\nF1 resolved at engine.rs:329.\n",
            }),
        );
        assert_eq!(result["isError"], true, "got {result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("'Findings'")
        );
    }

    /// The escape hatch the error message advertises: a walk that genuinely
    /// turned nothing up is still a walk, and "none" is how you say so.
    #[test]
    fn none_counts_as_content() {
        let result = call(
            &with_sections(&["Dismissed"]),
            json!({ "outcome": "approved", "summary": "Dismissed: none" }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// Review of #95, NF1: a finding that *starts with* another section's
    /// name is a finding, not that section's heading. Reading it as one
    /// both cut "Findings" short — rejecting a report whose Findings
    /// section is plainly there, with no way for the model to see why —
    /// and credited a walk nobody wrote.
    #[test]
    fn a_finding_that_opens_with_a_section_name_is_not_a_heading() {
        let stage = with_sections(&["Side effects", "Findings"]);
        let result = call(
            &stage,
            json!({
                "outcome": "changes_requested",
                "summary": "## Side effects\nOne INSERT, recorded once.\n\n                            ## Findings\n                            - Side effects of the retry are not covered by a test (mcp.rs:226).\n                            - States is missing a way out.\n",
            }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// Review of #95, NF3: the mirror image, and it lands on the very lap
    /// this issue is about. Seven bullets under "Prior findings", each
    /// naming a section, must not satisfy seven walks nobody did.
    #[test]
    fn bullets_under_prior_findings_do_not_satisfy_the_walks() {
        let stage = with_sections(&["Reviewed", "Findings", "States", "Messages"]);
        let result = call(
            &stage,
            json!({
                "outcome": "approved",
                "summary": "## Prior findings\n                            - Reviewed at abc123 previously.\n                            - Findings F1 resolved at engine.rs:329.\n                            - States handling now atomic.\n                            - Messages text fixed.\n",
            }),
        );
        assert_eq!(result["isError"], true, "got {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        for section in ["'Reviewed'", "'Findings'", "'States'", "'Messages'"] {
            assert!(text.contains(section), "{section} missing from {text}");
        }
    }

    /// A bulleted heading counts, including with its content on the same
    /// line — the format `reviewer-system.md` lists the walks in, and so
    /// the one a reviewer mirrors. Round 2 of #95's review caught an
    /// earlier bullet rule rejecting every section of a report written
    /// this way, while telling it the sections were missing.
    #[test]
    fn bulleted_headings_count_with_or_without_inline_content() {
        for summary in [
            "- Findings\n  nothing blocking\n",
            "- **Findings:** none blocking\n",
            "- Findings: none blocking\n",
            "* **Findings.** none blocking\n",
        ] {
            let result = call(
                &with_sections(&["Findings"]),
                json!({ "outcome": "approved", "summary": summary }),
            );
            assert_eq!(
                result["isError"], false,
                "summary {summary:?} gave {result}"
            );
        }
    }

    /// A report that mixes `##` headings with bulleted walks is still one
    /// report; no section may fall out over how it was written.
    #[test]
    fn a_report_mixing_heading_styles_is_accepted() {
        let result = call(
            &with_sections(&["Reviewed", "Side effects", "Findings"]),
            json!({
                "outcome": "approved",
                "summary": "## Reviewed\n30161d9\n\n\
                            - **Side effects:** one INSERT, recorded once.\n\n\
                            ## Findings\nnone blocking\n",
            }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// The commit line the prompt asks for, written with the colon the
    /// prompt shows. The bare `Reviewed 30161d9` form counted for a
    /// while; the test below says why it no longer does.
    #[test]
    fn the_commit_line_counts_as_the_reviewed_section() {
        for summary in ["## Reviewed: 30161d9\n", "## Reviewed\n30161d9\n"] {
            let result = call(
                &with_sections(&["Reviewed"]),
                json!({ "outcome": "approved", "summary": summary }),
            );
            assert_eq!(
                result["isError"], false,
                "summary {summary:?} gave {result}"
            );
        }
    }

    /// A terse one-word remainder used to count as content, so that a bare
    /// `Reviewed 30161d9` would match. On a re-review that credited whole
    /// walks to lines about the *previous* lap: "- Findings resolved."
    /// under "Prior findings" is the same shape as a commit line.
    #[test]
    fn a_terse_line_about_a_previous_lap_does_not_satisfy_a_walk() {
        let result = call(
            &with_sections(&["Findings", "States"]),
            json!({
                "outcome": "approved",
                "summary": "## Prior findings\n- Findings resolved.\n- States reworked.\n",
            }),
        );
        assert_eq!(result["isError"], true, "got {result}");
    }

    /// The suffix direction of the same guard — `Prior findings` is the
    /// prefix case, covered above. Round 3 of #95's review found the
    /// one-word rule had dropped this while the doc comment still
    /// promised it, and no test looked.
    #[test]
    fn a_word_that_merely_begins_with_a_section_name_is_not_a_heading() {
        for (summary, section) in [
            ("Findingsomething", "Findings"),
            ("Statesman", "States"),
            ("MessagesController.rs", "Messages"),
        ] {
            let result = call(
                &with_sections(&[section]),
                json!({ "outcome": "approved", "summary": summary }),
            );
            assert_eq!(
                result["isError"], true,
                "{summary:?} should not satisfy {section:?}: {result}"
            );
        }
    }

    /// A terse line inside a section can read as the heading of a walk
    /// written earlier in the report. Treating it as a boundary left the
    /// enclosing section looking empty, so a report was rejected for a
    /// section plainly there with content under it (review of #95, round
    /// 3) — the one failure this rule must never produce.
    #[test]
    fn a_line_naming_an_earlier_section_does_not_empty_the_one_it_sits_in() {
        let result = call(
            &with_sections(&["Old behaviour", "Findings", "Dismissed"]),
            json!({
                "outcome": "approved",
                "summary": "## Old behaviour\nNothing stops being observable.\n\n                            ## Findings\nnone blocking\n\n                            ## Dismissed\nOld behaviour preserved\n",
            }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// Review of #95, NF2: the sections are handed to the model as an
    /// ordered list, so it numbers them back. Before the fix that matched
    /// nothing at all — the whole report was rejected twice and then
    /// accepted with every section named as missing.
    #[test]
    fn numbered_and_hyphenated_headings_match() {
        let result = call(
            &with_sections(&["Reviewed", "Side effects"]),
            json!({
                "outcome": "approved",
                "summary": "### 1. Reviewed\n5d9cd0a\n\n### 2. Side-effects\nOne INSERT.\n",
            }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// A stage that asks for nothing is every stage that predates #95, and
    /// an empty summary stays acceptable there.
    #[test]
    fn a_stage_requiring_no_sections_accepts_any_summary() {
        let result = call(&outcomes(), json!({ "outcome": "approved", "summary": "" }));
        assert_eq!(result["isError"], false, "got {result}");
    }

    /// Parking a turn costs a human; two rejections don't. After the second
    /// the report is taken as it stands, with the gap named in the reply so
    /// it lands on the task's timeline.
    #[test]
    fn a_third_attempt_is_accepted_with_the_gap_named() {
        let stage = with_sections(&["Findings", "States"]);
        let thin = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "outcome": "approved", "summary": "Findings: none" },
            },
        });
        let responses = session(&stage, &[thin.clone(), thin.clone(), thin]);
        assert_eq!(
            responses[0]["result"]["isError"], true,
            "{:?}",
            responses[0]
        );
        assert_eq!(
            responses[1]["result"]["isError"], true,
            "{:?}",
            responses[1]
        );
        let third = &responses[2]["result"];
        assert_eq!(third["isError"], false, "got {third}");
        let text = third["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("'States'"), "got {text}");
        assert!(text.contains("attempt 3"), "got {text}");
    }

    /// The attempt number in that reply is the attempt it really is
    /// (review of #95): saturating the counter made the fourth and tenth
    /// thin report both claim to be the third.
    #[test]
    fn later_thin_reports_are_numbered_honestly() {
        let stage = with_sections(&["States"]);
        let thin = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "outcome": "approved", "summary": "nothing to see" },
            },
        });
        let responses = session(&stage, &[thin.clone(), thin.clone(), thin.clone(), thin]);
        let fourth = &responses[3]["result"];
        assert_eq!(fourth["isError"], false, "got {fourth}");
        let text = fourth["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("attempt 4"), "got {text}");
    }

    /// The allowance is for a model that can't satisfy the rule, not for one
    /// that gets there in the end: a complete report after two rejections is
    /// accepted as a complete report, with nothing said about the gap.
    #[test]
    fn a_report_that_arrives_complete_after_rejections_is_accepted_cleanly() {
        let stage = with_sections(&["Findings", "States"]);
        let thin = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "outcome": "approved", "summary": "Findings: none" },
            },
        });
        let complete = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": {
                    "outcome": "approved",
                    "summary": "Findings: none\nStates: no new state",
                },
            },
        });
        let responses = session(&stage, &[thin.clone(), thin, complete]);
        let third = &responses[2]["result"];
        assert_eq!(third["isError"], false, "got {third}");
        let text = third["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("still missing"), "got {text}");
    }

    /// The count is per section-rejection, not per call: an off-list outcome
    /// is a different mistake and must not spend the allowance.
    #[test]
    fn an_off_list_outcome_does_not_consume_the_section_allowance() {
        let stage = with_sections(&["States"]);
        let bad_outcome = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "outcome": "lgtm", "summary": "" },
            },
        });
        let thin = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "outcome": "approved", "summary": "nothing to see" },
            },
        });
        let responses = session(
            &stage,
            &[bad_outcome.clone(), bad_outcome, thin.clone(), thin],
        );
        for (index, response) in responses.iter().enumerate() {
            assert_eq!(
                response["result"]["isError"], true,
                "response {index}: {response}"
            );
        }
    }

    /// The sections are generated into the tool's own schema, so a model
    /// knows what is expected before its first call rather than learning it
    /// from a rejection.
    #[test]
    fn tools_list_states_the_required_sections() {
        let responses = session(
            &with_sections(&["Findings", "Dismissed"]),
            &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
        );
        let tool = &responses[0]["result"]["tools"][0];
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("Findings, Dismissed"), "{description}");
        let summary = tool["inputSchema"]["properties"]["summary"]["description"]
            .as_str()
            .unwrap();
        assert!(summary.contains("Findings, Dismissed"), "{summary}");
        assert!(summary.contains("you settle on 'outcome'"), "{summary}");
    }

    #[test]
    fn any_outcome_is_accepted_when_the_stage_declares_none() {
        let result = call(
            &no_outcomes(),
            json!({ "outcome": "blocked", "summary": "no network" }),
        );
        assert_eq!(result["isError"], false, "got {result}");
    }

    #[test]
    fn an_unknown_method_is_a_jsonrpc_error() {
        let responses = session(
            &outcomes(),
            &[json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" })],
        );
        assert_eq!(responses[0]["error"]["code"], -32601);
    }

    #[test]
    fn an_unparseable_line_is_a_jsonrpc_parse_error() {
        let mut output = Vec::new();
        serve(&outcomes(), &b"not json\n"[..], &mut output).unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["error"]["code"], -32700);
        assert!(response["id"].is_null());
    }

    #[test]
    fn blank_lines_are_ignored() {
        let mut output = Vec::new();
        serve(&outcomes(), &b"\n   \n"[..], &mut output).unwrap();
        assert!(output.is_empty(), "got {output:?}");
    }
}
