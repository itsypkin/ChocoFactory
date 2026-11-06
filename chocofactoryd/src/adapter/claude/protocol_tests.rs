//! What `normalize` refuses to treat as evidence (the `ProtocolError`
//! rules), and what it must keep accepting. Every line is a whole,
//! production-shaped native line; a malformed case changes only the field
//! under test.

use std::collections::HashMap;

use chocofactory_core::mcp::qualified_report_outcome_tool_name;
use chocofactory_core::models::EventType;
use serde_json::{Value, json};

use super::real_capture;
use super::{AgentEvent, BillingMode, TurnUsage, UsageCounting, normalize};

const SID: &str = "6901aab2-ef02-47d9-9634-d7883eb42636";
const CALL_A: &str = "toolu_01A1jR6fBWvWfbjeVUvegCuS";
const CALL_B: &str = "toolu_01SXCVvusWWEwzipBvQvgeBv";
const SUB_PARENT: &str = "toolu_01W3VQxTmyXy3B7b1bmz9YaX";

fn parse(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

type Names = HashMap<String, String>;

/// One line through a fresh normalizer.
fn norm(line: &Value) -> Vec<AgentEvent> {
    normalize(line, &mut Names::new(), &mut BillingMode::Unknown)
}

fn norm_with(line: &Value, names: &mut Names) -> Vec<AgentEvent> {
    normalize(line, names, &mut BillingMode::Unknown)
}

fn with(mut line: Value, key: &str, value: Value) -> Value {
    line[key] = value;
    line
}

fn without(mut line: Value, key: &str) -> Value {
    line.as_object_mut().unwrap().remove(key);
    line
}

fn report_input() -> Value {
    json!({"outcome": "approved", "summary": "## Findings\nnone"})
}

fn tool_use(id: &str, name: &str) -> Value {
    json!({"type":"tool_use","id":id,"name":name,"input":report_input(),"caller":{"type":"direct"}})
}

fn assistant(blocks: Vec<Value>) -> Value {
    json!({
        "type": "assistant",
        "message": {
            "model": "claude-haiku-5-5", "id": "msg_011CgnRbZkz8rWCJeAsbHoez",
            "type": "message", "role": "assistant", "content": blocks,
        },
        "session_id": SID,
        "uuid": "93bc5eef-16e9-4158-948f-b3b21bcf6ed7",
    })
}

fn report_call(id: &str) -> Value {
    assistant(vec![tool_use(id, &qualified_report_outcome_tool_name())])
}

/// The success form the real CLI sends: no `is_error`, array content.
fn success_block(id: &str) -> Value {
    json!({"tool_use_id": id, "type": "tool_result",
           "content": [{"type": "text", "text": "Accepted outcome 'approved'."}]})
}

fn user(blocks: Vec<Value>) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": blocks},
           "session_id": SID, "uuid": "8263ec55-f8d8-4b47-abe7-3112775f9a27"})
}

const SUCCESS_OUTPUT: &str = r#"[{"text":"Accepted outcome 'approved'.","type":"text"}]"#;

fn result_line() -> Value {
    parse(
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","duration_ms":5422,"num_turns":4,"total_cost_usd":0.00641,"usage":{"input_tokens":6,"output_tokens":455,"cache_read_input_tokens":55047,"cache_creation_input_tokens":28159},"modelUsage":{"claude-haiku-5-5":{"inputTokens":6,"outputTokens":455,"cacheReadInputTokens":55047,"cacheCreationInputTokens":28159,"costUSD":0.00641}},"session_id":"6901aab2-ef02-47d9-9634-d7883eb42636"}"#,
    )
}

fn known_usage() -> TurnUsage {
    match norm(&result_line()).as_slice() {
        [
            AgentEvent::TurnCompleted {
                is_error: false,
                usage,
            },
        ] => {
            assert_eq!(usage.cost_usd, Some(0.00641));
            assert_eq!(usage.tokens.output, Some(455));
            usage.clone()
        }
        other => panic!("the healthy result line gave {other:?}"),
    }
}

fn main_call(id: &str) -> AgentEvent {
    AgentEvent::ToolCall {
        tool_use_id: id.to_string(),
        tool: qualified_report_outcome_tool_name(),
        input: report_input(),
    }
}

fn pe(detail: &str) -> AgentEvent {
    AgentEvent::ProtocolError {
        message: format!("malformed CLI output: {detail}"),
    }
}

fn parent_pe(kind: &str) -> AgentEvent {
    pe(&format!(
        "parent_tool_use_id is {kind}, not a non-empty string or null"
    ))
}

fn wrap(parent: &str, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
    events
        .into_iter()
        .map(|event| AgentEvent::Subagent {
            parent_tool_use_id: parent.to_string(),
            event: Box::new(event),
        })
        .collect()
}

fn has_call(events: &[AgentEvent]) -> bool {
    events.iter().any(|event| match event {
        AgentEvent::ToolCall { .. } => true,
        AgentEvent::Subagent { event, .. } => has_call(std::slice::from_ref(event)),
        _ => false,
    })
}

fn has_result(events: &[AgentEvent]) -> bool {
    events.iter().any(|event| match event {
        AgentEvent::ToolResult { .. } => true,
        AgentEvent::Subagent { event, .. } => has_result(std::slice::from_ref(event)),
        _ => false,
    })
}

/// Every shape a field can have other than a usable non-empty string:
/// (value, `None` meaning absent; the description `kind` gives it).
fn bad_strings() -> Vec<(Option<Value>, &'static str)> {
    vec![
        (None, "missing"),
        (Some(Value::Null), "null"),
        (Some(json!(true)), "a boolean"),
        (Some(json!(7)), "a number"),
        (Some(json!("")), "an empty string"),
        (Some(json!(["toolu_01x"])), "an array"),
        (Some(json!({"id": "toolu_01x"})), "an object"),
    ]
}

fn set_or_remove(mut block: Value, key: &str, value: &Option<Value>) -> Value {
    match value {
        Some(v) => block[key] = v.clone(),
        None => {
            block.as_object_mut().unwrap().remove(key);
        }
    }
    block
}

// ---------------------------------------------------------------- events

#[test]
fn a_top_level_protocol_error_is_an_error_event_with_the_marker() {
    let event = pe("a tool_use block's id is null");
    assert_eq!(event.event_type(), EventType::Error);
    assert_eq!(
        event.payload(),
        json!({"message": "malformed CLI output: a tool_use block's id is null",
               "protocol_error": true})
    );
}

#[test]
fn a_wrapped_protocol_error_keeps_the_marker_and_the_parent() {
    let event = AgentEvent::Subagent {
        parent_tool_use_id: SUB_PARENT.to_string(),
        event: Box::new(pe("a tool_use block's id is null")),
    };
    assert_eq!(event.event_type(), EventType::Error);
    assert_eq!(
        event.payload(),
        json!({"message": "malformed CLI output: a tool_use block's id is null",
               "protocol_error": true, "parent_tool_use_id": SUB_PARENT})
    );
}

// ------------------------------------------------------------------- NA1

#[test]
fn na1_parent_scope() {
    let line = report_call(CALL_A);
    let main = vec![main_call(CALL_A)];

    assert_eq!(norm(&line), main, "main events (parent missing)");
    assert_eq!(
        norm(&with(line.clone(), "parent_tool_use_id", Value::Null)),
        main,
        "main events (parent null)"
    );
    assert_eq!(
        norm(&with(line.clone(), "parent_tool_use_id", json!("toolu_x"))),
        wrap("toolu_x", main.clone())
    );
    assert_eq!(
        norm(&with(line.clone(), "parent_tool_use_id", json!(SUB_PARENT))),
        wrap(SUB_PARENT, main)
    );

    for (parent, kind) in [
        (json!(7), "a number"),
        (json!(true), "a boolean"),
        (json!({}), "an object"),
        (json!([]), "an array"),
        (json!(""), "an empty string"),
    ] {
        let events = norm(&with(line.clone(), "parent_tool_use_id", parent.clone()));
        assert!(
            !has_call(&events),
            "no ToolCall for a wrong-type parent {parent}: {events:?}"
        );
        assert_eq!(events, vec![parent_pe(kind)], "parent {parent}");
    }
}

#[test]
fn na1_lines_with_no_events_are_not_refused_for_their_parent() {
    let eventless = [
        json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1788000000},
               "session_id": SID}),
        json!({"type":"stream_event","session_id": SID}),
        json!({"type":"control_response","response":{"subtype":"success","request_id":"r1"}}),
        assistant(vec![]),
        user(vec![json!({"type":"text","text":"hi"})]),
    ];
    for line in eventless {
        for parent in [json!(7), json!(true), json!({}), json!([]), json!("")] {
            let events = norm(&with(line.clone(), "parent_tool_use_id", parent.clone()));
            assert_eq!(events, vec![], "no events for {line} with parent {parent}");
        }
    }
}

// ------------------------------------------------------------------- NA2

#[test]
fn na2_each_tool_use_field_is_validated() {
    let tool = qualified_report_outcome_tool_name();
    let mut bad = bad_strings();
    // A non-empty value of the two container types also reaches the guard.
    bad.push((Some(json!(["a"])), "an array"));
    for field in ["id", "name"] {
        for (value, kind) in &bad {
            let mut names = Names::new();
            let block = set_or_remove(tool_use(CALL_A, &tool), field, value);
            let events = norm_with(&assistant(vec![block]), &mut names);
            assert!(!has_call(&events), "no ToolCall: {field} {value:?}");
            assert_eq!(
                events,
                vec![pe(&format!("a tool_use block's {field} is {kind}"))],
                "{field} {value:?}"
            );
            assert!(names.is_empty(), "a malformed block registers nothing");
        }
    }
}

#[test]
fn na2_a_whitespace_only_string_is_a_valid_name_and_id() {
    let events = norm(&assistant(vec![tool_use(" ", " ")]));
    assert_eq!(
        events,
        vec![AgentEvent::ToolCall {
            tool_use_id: " ".to_string(),
            tool: " ".to_string(),
            input: report_input(),
        }]
    );
}

#[test]
fn na2_id_is_checked_before_name_and_only_one_error_is_emitted() {
    let mut names = Names::new();
    let block = json!({"type":"tool_use","id":null,"name":7,"input":{}});
    let events = norm_with(&assistant(vec![block]), &mut names);
    assert!(!has_call(&events), "no ToolCall");
    assert_eq!(events, vec![pe("a tool_use block's id is null")]);
    assert!(names.is_empty());
}

// ------------------------------------------------------------------- NA3

#[test]
fn na3_a_tool_result_needs_a_non_empty_string_id() {
    for (value, kind) in bad_strings() {
        let mut names = Names::new();
        norm_with(&report_call(CALL_A), &mut names);
        let block = set_or_remove(success_block(CALL_A), "tool_use_id", &value);
        let events = norm_with(&user(vec![block]), &mut names);
        assert!(
            !has_result(&events),
            "ProtocolError, no ToolResult for {value:?}: {events:?}"
        );
        assert_eq!(
            events,
            vec![pe(&format!("a tool_result block's tool_use_id is {kind}"))],
            "{value:?}"
        );
    }
}

#[test]
fn na3_id_is_checked_before_the_flag() {
    let block = json!({"type":"tool_result","tool_use_id":null,"is_error":"x","content":"c"});
    let events = norm(&user(vec![block]));
    assert_eq!(
        events,
        vec![pe("a tool_result block's tool_use_id is null")]
    );
}

// ------------------------------------------------------------------- NA4

fn result_for(flag: Option<Value>, content: Value) -> Vec<AgentEvent> {
    let mut names = Names::new();
    norm_with(&report_call(CALL_A), &mut names);
    let mut block = json!({"tool_use_id": CALL_A, "type": "tool_result", "content": content});
    if let Some(flag) = flag {
        block["is_error"] = flag;
    }
    norm_with(&user(vec![block]), &mut names)
}

fn tool_result(output: &str, is_error: bool) -> AgentEvent {
    AgentEvent::ToolResult {
        tool_use_id: CALL_A.to_string(),
        tool: qualified_report_outcome_tool_name(),
        output: output.to_string(),
        is_error,
    }
}

#[test]
fn na4_tool_result_flag() {
    let array = json!([{"type": "text", "text": "Accepted outcome 'approved'."}]);

    // The CLI leaves the flag off a successful result: that is success.
    assert_eq!(
        result_for(None, array.clone()),
        vec![tool_result(SUCCESS_OUTPUT, false)],
        "success"
    );
    assert_eq!(
        result_for(Some(json!(false)), array.clone()),
        vec![tool_result(SUCCESS_OUTPUT, false)],
        "explicit false is success"
    );
    // true is a rejection whatever the content form.
    assert_eq!(
        result_for(
            Some(json!(true)),
            json!("Your report is missing 'Findings'.")
        ),
        vec![tool_result("Your report is missing 'Findings'.", true)]
    );
    assert_eq!(
        result_for(Some(json!(true)), array),
        vec![tool_result(SUCCESS_OUTPUT, true)],
        "true is never turned into success by array content"
    );

    for (flag, kind) in [
        (json!(null), "null"),
        (json!("true"), "a string"),
        (json!(1), "a number"),
        (json!({}), "an object"),
        (json!([]), "an array"),
        (json!(""), "an empty string"),
    ] {
        let events = result_for(Some(flag.clone()), json!("c"));
        assert!(!has_result(&events), "no ToolResult for {flag}: {events:?}");
        assert_eq!(
            events,
            vec![pe(&format!(
                "a tool_result block's is_error is {kind}, not true or false"
            ))],
            "ProtocolError for {flag}"
        );
    }
}

#[test]
fn na4_absent_content_stays_empty_output() {
    let mut names = Names::new();
    norm_with(&report_call(CALL_A), &mut names);
    let block = json!({"tool_use_id": CALL_A, "type": "tool_result"});
    assert_eq!(
        norm_with(&user(vec![block]), &mut names),
        vec![tool_result("", false)]
    );
}

// ------------------------------------------------------------------- NA5

#[test]
fn na5_result_flag() {
    let usage = known_usage();
    assert_eq!(
        norm(&result_line()),
        vec![AgentEvent::TurnCompleted {
            is_error: false,
            usage: usage.clone()
        }]
    );

    let failed = with(
        with(result_line(), "is_error", json!(true)),
        "result",
        json!("API Error: boom"),
    );
    assert_eq!(
        norm(&failed),
        vec![
            AgentEvent::Error {
                message: "API Error: boom".to_string()
            },
            AgentEvent::TurnCompleted {
                is_error: true,
                usage: usage.clone()
            },
        ]
    );

    let mut cases = vec![(without(result_line(), "is_error"), "missing")];
    for (flag, kind) in [
        (json!(null), "null"),
        (json!("false"), "a string"),
        (json!(0), "a number"),
        (json!([]), "an array"),
        (json!({}), "an object"),
        (json!(""), "an empty string"),
    ] {
        cases.push((with(result_line(), "is_error", flag), kind));
    }
    for (line, kind) in cases {
        let events = norm(&line);
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::TurnCompleted { is_error: true, .. })
            ),
            "TurnCompleted is_error true ({kind}): {events:?}"
        );
        assert_eq!(
            events,
            vec![
                pe(&format!(
                    "a result line's is_error is {kind}, not true or false"
                )),
                AgentEvent::TurnCompleted {
                    is_error: true,
                    usage: usage.clone()
                },
            ],
            "{kind}"
        );
    }
}

// ------------------------------------------------------------------- NA6

#[test]
fn na6_an_id_may_repeat_with_the_same_name() {
    let mut names = Names::new();
    let first = norm_with(&report_call(CALL_A), &mut names);
    let second = norm_with(&report_call(CALL_A), &mut names);
    assert_eq!(first, vec![main_call(CALL_A)]);
    assert_eq!(second, vec![main_call(CALL_A)]);
}

#[test]
fn na6_an_id_may_not_change_its_name_across_lines() {
    let mut names = Names::new();
    norm_with(&report_call(CALL_A), &mut names);
    let second = norm_with(&assistant(vec![tool_use(CALL_A, "Bash")]), &mut names);
    assert!(!has_call(&second), "no second ToolCall: {second:?}");
    assert_eq!(
        second,
        vec![pe(&format!(
            "tool_use id '{CALL_A}' was already used for tool '{}', now for 'Bash'",
            qualified_report_outcome_tool_name()
        ))]
    );
    // The first mapping still names the result.
    let result = norm_with(&user(vec![success_block(CALL_A)]), &mut names);
    assert_eq!(result, vec![tool_result(SUCCESS_OUTPUT, false)]);
}

#[test]
fn na6_an_id_may_not_change_its_name_within_one_line() {
    let mut names = Names::new();
    let tool = qualified_report_outcome_tool_name();
    let line = assistant(vec![tool_use(CALL_A, &tool), tool_use(CALL_A, "Bash")]);
    let events = norm_with(&line, &mut names);
    assert_eq!(
        events,
        vec![
            main_call(CALL_A),
            pe(&format!(
                "tool_use id '{CALL_A}' was already used for tool '{tool}', now for 'Bash'"
            )),
        ]
    );
    assert_eq!(names.get(CALL_A), Some(&tool));
}

// ------------------------------------------------------------------- NA7

#[test]
fn na7_real_claude_envelopes() {
    let mut names = Names::new();
    let mut billing = BillingMode::Unknown;
    let mut feed = |line: &str| normalize(&parse(line), &mut names, &mut billing);
    let tool = qualified_report_outcome_tool_name();

    assert_eq!(
        feed(real_capture::CALL_REJECTED),
        vec![AgentEvent::ToolCall {
            tool_use_id: CALL_A.to_string(),
            tool: tool.clone(),
            input: json!({"outcome": "approved", "summary": "ok"}),
        }]
    );
    assert_eq!(
        feed(real_capture::RESULT_REJECTED),
        vec![AgentEvent::ToolResult {
            tool_use_id: CALL_A.to_string(),
            tool: tool.clone(),
            output: "Your report's 'summary' is missing 'Findings': each required section needs a heading line with something under it (a section with genuinely nothing in it says \"<section>: none\"). Required sections, in order: Findings. Finish the walks you skipped, then call this tool again with the complete report.".to_string(),
            is_error: true,
        }]
    );
    assert_eq!(
        feed(real_capture::CALL_ACCEPTED),
        vec![AgentEvent::ToolCall {
            tool_use_id: CALL_B.to_string(),
            tool: tool.clone(),
            input: json!({"outcome": "approved", "summary": "## Findings\nnone"}),
        }]
    );
    assert_eq!(
        feed(real_capture::RESULT_ACCEPTED),
        vec![AgentEvent::ToolResult {
            tool_use_id: CALL_B.to_string(),
            tool,
            output: SUCCESS_OUTPUT.to_string(),
            is_error: false,
        }]
    );
    let end = feed(real_capture::RESULT_LINE);
    match end.as_slice() {
        [
            AgentEvent::TurnCompleted {
                is_error: false,
                usage,
            },
        ] => {
            assert_eq!(usage.cost_usd, Some(0.006410370000000001));
            assert_eq!(usage.tokens.input, Some(6));
            assert_eq!(usage.tokens.output, Some(455));
            assert_eq!(usage.tokens.cache_read, Some(55047));
            assert_eq!(usage.tokens.cache_write, Some(28159));
            assert_eq!(usage.wall_time_ms, Some(5422));
            assert_eq!(usage.model_turns, Some(4));
            assert_eq!(usage.counting, UsageCounting::CumulativePerConversation);
            let models = usage.models.as_ref().unwrap();
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].model, "claude-haiku-5-5");
            assert_eq!(models[0].tokens.output, Some(455));
        }
        other => panic!("the real result line gave {other:?}"),
    }
}

// ------------------------------------------------------- mixed and scoped

fn mixed_assistant() -> (Value, Vec<AgentEvent>) {
    let tool = qualified_report_outcome_tool_name();
    let line = assistant(vec![
        json!({"type":"text","text":"reporting"}),
        set_or_remove(tool_use(CALL_A, &tool), "id", &Some(Value::Null)),
        tool_use(CALL_B, &tool),
    ]);
    let events = vec![
        AgentEvent::AssistantMessage {
            text: "reporting".to_string(),
        },
        pe("a tool_use block's id is null"),
        main_call(CALL_B),
    ];
    (line, events)
}

fn mixed_user() -> (Value, Vec<AgentEvent>) {
    let mut bad = success_block(CALL_B);
    bad["is_error"] = Value::Null;
    let line = user(vec![success_block(CALL_A), bad]);
    let events = vec![
        tool_result(SUCCESS_OUTPUT, false),
        pe("a tool_result block's is_error is null, not true or false"),
    ];
    (line, events)
}

fn seeded() -> Names {
    let mut names = Names::new();
    norm_with(&report_call(CALL_A), &mut names);
    norm_with(&report_call(CALL_B), &mut names);
    names
}

#[test]
fn a_malformed_block_rejects_only_that_block() {
    let (line, events) = mixed_assistant();
    let mut names = Names::new();
    assert_eq!(norm_with(&line, &mut names), events);
    assert_eq!(names.len(), 1, "only the valid block registered");
    assert!(names.contains_key(CALL_B));

    let (line, events) = mixed_user();
    assert_eq!(norm_with(&line, &mut seeded()), events);
}

#[test]
fn a_valid_parent_wraps_the_diagnostic_too() {
    let (line, events) = mixed_assistant();
    let line = with(line, "parent_tool_use_id", json!(SUB_PARENT));
    assert_eq!(norm(&line), wrap(SUB_PARENT, events));

    let (line, events) = mixed_user();
    let line = with(line, "parent_tool_use_id", json!(SUB_PARENT));
    assert_eq!(norm_with(&line, &mut seeded()), wrap(SUB_PARENT, events));
}

#[test]
fn a_malformed_parent_turns_a_mixed_line_into_one_diagnostic() {
    let (line, _) = mixed_assistant();
    let line = with(line, "parent_tool_use_id", json!(7));
    assert_eq!(norm(&line), vec![parent_pe("a number")]);

    let (line, _) = mixed_user();
    let line = with(line, "parent_tool_use_id", json!({}));
    assert_eq!(
        norm_with(&line, &mut seeded()),
        vec![parent_pe("an object")]
    );
}

#[test]
fn map_rollback_for_a_malformed_parent() {
    let mut names = Names::new();
    names.insert("toolu_01Pre".to_string(), "Bash".to_string());
    let before = names.clone();

    let line = with(
        assistant(vec![tool_use(CALL_B, "Read")]),
        "parent_tool_use_id",
        json!(7),
    );
    let events = norm_with(&line, &mut names);
    assert_eq!(events, vec![parent_pe("a number")]);
    assert_eq!(
        names, before,
        "malformed parent leaves tool_names unchanged"
    );

    // The id is still free: a correctly scoped call under another name is
    // not a conflict with the dropped one.
    let events = norm_with(&assistant(vec![tool_use(CALL_B, "Write")]), &mut names);
    assert_eq!(
        events,
        vec![AgentEvent::ToolCall {
            tool_use_id: CALL_B.to_string(),
            tool: "Write".to_string(),
            input: report_input(),
        }]
    );
}

/// Eventful lines of every type, each with the names it needs seeded.
fn eventful_lines() -> Vec<(&'static str, Value)> {
    vec![
        ("assistant", report_call(CALL_B)),
        ("user", user(vec![success_block(CALL_A)])),
        ("result", result_line()),
        ("result without a flag", without(result_line(), "is_error")),
        (
            "init",
            json!({"type":"system","subtype":"init","session_id":SID,"apiKeySource":"none","model":"m"}),
        ),
        (
            "failed control_response",
            json!({"type":"control_response","response":{"subtype":"error","request_id":"r","error":"nope"}}),
        ),
        (
            "rejected rate_limit_event",
            json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected"},"session_id":SID}),
        ),
        (
            "assistant interruption",
            with(
                with(
                    assistant(vec![json!({"type":"text","text":"You've hit your limit"})]),
                    "error",
                    json!("rate_limit"),
                ),
                "apiErrorStatus",
                json!(429),
            ),
        ),
    ]
}

#[test]
fn parent_attribution_applies_to_every_eventful_line_type() {
    for (what, line) in eventful_lines() {
        let main = norm_with(&line, &mut seeded());
        assert!(!main.is_empty(), "{what} must be eventful");

        assert_eq!(
            norm_with(
                &with(line.clone(), "parent_tool_use_id", Value::Null),
                &mut seeded()
            ),
            main,
            "{what}: null parent is main"
        );
        assert_eq!(
            norm_with(
                &with(line.clone(), "parent_tool_use_id", json!(SUB_PARENT)),
                &mut seeded()
            ),
            wrap(SUB_PARENT, main.clone()),
            "{what}: valid parent wraps every event"
        );
        let dropped = norm_with(
            &with(line.clone(), "parent_tool_use_id", json!(7)),
            &mut seeded(),
        );
        assert_eq!(
            dropped,
            vec![parent_pe("a number")],
            "{what}: malformed parent"
        );
    }
}

#[test]
fn a_malformed_parent_init_does_not_change_billing() {
    let mut billing = BillingMode::Unknown;
    let line = with(
        json!({"type":"system","subtype":"init","session_id":SID,"apiKeySource":"none"}),
        "parent_tool_use_id",
        json!(7),
    );
    let events = normalize(&line, &mut Names::new(), &mut billing);
    assert_eq!(events, vec![parent_pe("a number")]);
    assert_eq!(billing, BillingMode::Unknown);
}

#[test]
fn background_tasks_changed_with_a_parent_stays_eventless() {
    let line = json!({"type":"system","subtype":"background_tasks_changed",
                      "tasks":[{"task_id":"a","task_type":"t","description":"a"}]});
    for parent in [json!(7), json!("toolu_x")] {
        let events = norm(&with(line.clone(), "parent_tool_use_id", parent));
        assert_eq!(events, vec![]);
    }
}

#[test]
fn a_sub_agents_malformed_result_ends_only_the_sub_agent() {
    let line = with(
        without(result_line(), "is_error"),
        "parent_tool_use_id",
        json!(SUB_PARENT),
    );
    let events = norm(&line);
    let usage = known_usage();
    assert_eq!(
        events,
        wrap(
            SUB_PARENT,
            vec![
                pe("a result line's is_error is missing, not true or false"),
                AgentEvent::TurnCompleted {
                    is_error: true,
                    usage
                }
            ]
        )
    );
    assert!(
        events
            .iter()
            .all(|e| matches!(e, AgentEvent::Subagent { .. })),
        "no main-agent completion"
    );
}
