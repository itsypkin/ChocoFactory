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
//! - **Stateless.** It never contacts `chocofactoryd`. The daemon already
//!   records every tool call an agent makes as a `tool_call` event, so it
//!   reads the verdict back off that timeline exactly as it already reads the
//!   final assistant text. That leaves nothing here to authenticate, no
//!   endpoint to add, and no shared state to race on.
//! - **Hand-rolled.** Four JSON-RPC methods over newline-delimited stdio, no
//!   MCP SDK. `serde_json` is already a dependency; a crate for one tool
//!   would not be.
//!
//! The allowed `outcome` values are passed in with `--outcomes` and come from
//! the stage's `on:` map, so this binary has no idea what a "reviewer" is —
//! every agent turn gets the same tool, and only the list differs.

use std::io::{BufRead, Write};

use chocofactory_core::mcp::{MCP_SERVER_NAME, REPORT_OUTCOME_TOOL_NAME};
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

/// Serves the tool over `input`/`output` until the client closes the stream.
///
/// Split from the subcommand entry point so tests can drive a whole session
/// over byte slices, the same reason `client`'s request builders are split
/// from their sending halves.
pub fn serve(
    outcomes: &[String],
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(outcomes, &line) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Answers one request line. `None` means "say nothing", which is required
/// rather than merely polite: a JSON-RPC *notification* has no `id`, and
/// replying to one is a protocol violation.
fn handle_line(outcomes: &[String], line: &str) -> Option<String> {
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
        "tools/list" => Ok(json!({ "tools": [tool_definition(outcomes)] })),
        "tools/call" => match call_tool(outcomes, request.get("params")) {
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
fn call_tool(outcomes: &[String], params: Option<&Value>) -> Result<Value, CallError> {
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

    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!("Recorded outcome '{outcome}'."),
        }],
        "isError": false,
    }))
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
fn tool_definition(outcomes: &[String]) -> Value {
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
        "Report a status for this stage. Optional — this stage does not route on reported \
         outcomes, so the report is recorded on the task's timeline but does not affect what \
         happens next."
            .to_string()
    } else {
        outcome_schema["enum"] = json!(outcomes);
        format!(
            "Report this stage's outcome so the workflow can route on it. Call this before you \
             end your turn. 'outcome' must be exactly one of: {}.",
            outcomes.join(", ")
        )
    };

    json!({
        "name": TOOL_NAME,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "outcome": outcome_schema,
                "summary": {
                    "type": "string",
                    "description": "Your reasoning, specific enough for whoever acts on this \
                                    next to work from without re-reading your whole turn. May be \
                                    empty when there is nothing to add.",
                },
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

    fn outcomes() -> Vec<String> {
        vec!["approved".to_string(), "changes_requested".to_string()]
    }

    /// Drives a whole session and returns one parsed response per line the
    /// server wrote — so a test can assert on what a client would actually
    /// see, including *how many* responses it got.
    fn session(outcomes: &[String], requests: &[Value]) -> Vec<Value> {
        let input = requests
            .iter()
            .map(|request| request.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = Vec::new();
        serve(outcomes, input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn call(outcomes: &[String], arguments: Value) -> Value {
        let responses = session(
            outcomes,
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
            &[],
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

    #[test]
    fn any_outcome_is_accepted_when_the_stage_declares_none() {
        let result = call(
            &[],
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
