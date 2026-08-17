//! Speaks just enough of `claude --print --output-format=stream-json
//! --input-format=stream-json [--resume <id>]` to stand in for the real
//! `claude` CLI (design §4, issue #42). `ClaudeAdapter::with_binary`
//! points at this instead of the real, billable `claude` subprocess for
//! e2e tests and manual smoke testing (`CHOKOFACTORY_CLAUDE_BINARY`).
//!
//! Protocol matches `chokofactoryd/tests/fixtures/fake_claude.py` (the
//! Python fixture this promotes to a first-class, interpreter-free
//! binary): every turn is echoed back as `echo:{text}` by default, or a
//! fixed string from `MOCK_CLAUDE_REPLY` if that's set. `--resume <id>`
//! reuses the given session id rather than minting a new one, matching
//! real `claude`'s resume contract closely enough for the idle/resume
//! tests in `session.rs`/`engine.rs`.
//!
//! Two further env knobs exist for the `agent_turn` `capture:` path (#45),
//! which a chat-shaped mock cannot exercise:
//!
//! - `MOCK_CLAUDE_ONESHOT=1` — exit after one turn instead of waiting for
//!   more input, the way a `coding`/`internal_review` stage's subprocess
//!   does. A capturing stage only concludes when its run goes idle, and a
//!   process that never exits is only reaped — which deliberately does
//!   *not* auto-advance. Mirrors `fake_claude_oneshot.py`.
//! - `MOCK_CLAUDE_TOOL_USE=1` — narrate, call a tool, receive its result,
//!   and only then answer. This is what every real turn that touches a
//!   tool looks like, and it is the shape that breaks a capture which
//!   concatenates everything the agent said rather than taking its final
//!   message.
//!
//! Every other flag (`--print`, `--input-format`, `--output-format`,
//! `--verbose`, `--model`, `--system-prompt`) is accepted but ignored —
//! this binary only needs to *emit* valid stream-json, not validate the
//! invocation.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};
use uuid::Uuid;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let session_id = resume_session_id(&args).unwrap_or_else(|| Uuid::new_v4().to_string());

    if !emit(&json!({
        "type": "system",
        "subtype": "init",
        "session_id": session_id,
    })) {
        return;
    }

    let reply_override = std::env::var("MOCK_CLAUDE_REPLY").ok();
    let oneshot = std::env::var_os("MOCK_CLAUDE_ONESHOT").is_some();
    let uses_tool = std::env::var_os("MOCK_CLAUDE_TOOL_USE").is_some();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line.expect("failed to read a line from stdin");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let turn: Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("received a non-JSON stdin turn ({err}): {line}"));
        let text = turn
            .pointer("/message/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("stdin turn missing /message/content/0/text: {turn}"));
        let reply = reply_override
            .clone()
            .unwrap_or_else(|| format!("echo:{text}"));

        if uses_tool {
            // The answer is the *last* message, not everything that was
            // said — narration first, then the tool round-trip.
            let narrated = emit(&json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "text", "text": "I'll read the diff first." },
                    { "type": "tool_use", "id": "toolu_1", "name": "Read",
                      "input": { "path": "a.rs" } },
                ]},
                "session_id": session_id,
            }));
            if !narrated {
                return;
            }
            let tool_result = emit(&json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1",
                      "content": "fn main() {}" },
                ]},
                "session_id": session_id,
            }));
            if !tool_result {
                return;
            }
        }

        let assistant_ok = emit(&json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": reply }] },
            "session_id": session_id,
        }));
        if !assistant_ok {
            return;
        }

        let result_ok = emit(&json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": reply,
            "session_id": session_id,
        }));
        if !result_ok {
            return;
        }

        if oneshot {
            return;
        }
    }
}

/// Extracts the value following a `--resume` flag, if present.
fn resume_session_id(args: &[String]) -> Option<String> {
    args.iter()
        .position(|a| a == "--resume")
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Writes one stream-json line to stdout, flushed immediately (the real
/// `ClaudeAdapter` reads line-by-line as output arrives, so a buffered-
/// but-unflushed line would just sit unseen). Returns `false` if the
/// write failed — e.g. the reader on the other end of the pipe is gone —
/// so the caller can stop instead of looping against a dead pipe.
fn emit(value: &Value) -> bool {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{value}").is_ok() && handle.flush().is_ok()
}
