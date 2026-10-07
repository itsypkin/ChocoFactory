//! What the adapters for the Pi coding-agent family share (`omp` today).
//!
//! Only the parts that are about the family's wire format and its
//! conventions live here: the line reader, pi-ai message and usage parsing,
//! session statistics, and reading repo instruction files. Spawning, the RPC
//! session and everything specific to one CLI stay in that CLI's own module.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use super::{AgentEvent, ModelUsage, TokenCounts};

// ---------------------------------------------------------------------------
// Line reader
// ---------------------------------------------------------------------------

/// Reads one frame line. Splits on LF (`\n`) only: a Unicode line or
/// paragraph separator (U+2028/U+2029) is legal inside a JSON string and must
/// not end the frame. Invalid UTF-8 is read lossily; the frame's JSON parse
/// decides whether the result is usable. `None` at end of stream.
pub async fn read_lf_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    if reader.read_until(b'\n', &mut bytes).await? == 0 {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

// ---------------------------------------------------------------------------
// pi-ai messages -> AgentEvents
// ---------------------------------------------------------------------------

/// Turns finished pi-ai messages (`message_end`) into events, remembering
/// each tool call's name so its result can be labelled.
#[derive(Default)]
pub struct MessageNormalizer {
    tool_names: HashMap<String, String>,
}

impl MessageNormalizer {
    /// `skip_tool` names a tool whose call and result are not reported here
    /// (the host `report_outcome` tool, which the adapter reports itself).
    pub fn normalize(&mut self, message: &Value, skip_tool: &str) -> Vec<AgentEvent> {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => self.assistant(message, skip_tool),
            Some("toolResult") => self.tool_result(message, skip_tool),
            _ => Vec::new(),
        }
    }

    fn assistant(&mut self, message: &Value, skip_tool: &str) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return events;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        events.push(AgentEvent::AssistantMessage {
                            text: text.to_string(),
                        });
                    }
                }
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                        events.push(AgentEvent::Thinking {
                            text: text.to_string(),
                        });
                    }
                }
                Some("toolCall") => {
                    let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    self.tool_names.insert(id.to_string(), name.to_string());
                    if name == skip_tool {
                        continue;
                    }
                    events.push(AgentEvent::ToolCall {
                        tool_use_id: id.to_string(),
                        tool: name.to_string(),
                        input: block.get("arguments").cloned().unwrap_or(Value::Null),
                    });
                }
                _ => {}
            }
        }
        events
    }

    fn tool_result(&mut self, message: &Value, skip_tool: &str) -> Vec<AgentEvent> {
        let Some(id) = message.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let tool = message
            .get("toolName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.tool_names.get(id).cloned())
            .unwrap_or_default();
        if tool == skip_tool {
            return Vec::new();
        }
        let output: String = message
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        vec![AgentEvent::ToolResult {
            tool_use_id: id.to_string(),
            tool,
            output,
            is_error: message
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }]
    }
}

// ---------------------------------------------------------------------------
// pi-ai usage
// ---------------------------------------------------------------------------

/// One assistant message's `usage`, with the model that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageUsage {
    /// `<provider>/<model>`.
    pub model: String,
    pub tokens: TokenCounts,
    pub total_tokens: u64,
    pub cost_total: Option<f64>,
}

impl MessageUsage {
    /// Tokens were used but no price came with them (a model omp has no
    /// price list for). Zero would be a lie, so this means "unknown".
    pub fn unpriced(&self) -> bool {
        self.cost_total.unwrap_or(0.0) == 0.0 && self.total_tokens > 0
    }
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// Parses an assistant message's pi-ai `Usage`. `input` already excludes
/// cache reads. `None` when the message has no usage object.
pub fn parse_message_usage(message: &Value) -> Option<MessageUsage> {
    let usage = message.get("usage").filter(|usage| usage.is_object())?;
    let provider = message
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Some(MessageUsage {
        model: format!("{provider}/{model}"),
        tokens: TokenCounts {
            input: u64_field(usage, "input"),
            output: u64_field(usage, "output"),
            cache_read: u64_field(usage, "cacheRead"),
            cache_write: u64_field(usage, "cacheWrite"),
        },
        total_tokens: u64_field(usage, "totalTokens").unwrap_or(0),
        cost_total: usage
            .get("cost")
            .and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64),
    })
}

fn add_opt(acc: &mut Option<u64>, n: Option<u64>) {
    if let Some(n) = n {
        *acc = Some(acc.unwrap_or(0).saturating_add(n));
    }
}

#[derive(Default)]
struct ModelAcc {
    tokens: TokenCounts,
    cost: Option<f64>,
    any_priced: bool,
    any_unpriced: bool,
}

/// The assistant messages of one turn: how many, and their usage by model.
#[derive(Default)]
pub struct TurnMessages {
    pub assistant_messages: u32,
    per_model: BTreeMap<String, ModelAcc>,
    any_priced: bool,
    any_unpriced: bool,
    /// The model of the last message that carried usage.
    pub last_model: Option<String>,
}

impl TurnMessages {
    pub fn add(&mut self, usage: Option<MessageUsage>) {
        self.assistant_messages += 1;
        let Some(usage) = usage else {
            return;
        };
        let unpriced = usage.unpriced();
        self.last_model = Some(usage.model.clone());
        if unpriced {
            self.any_unpriced = true;
        } else {
            self.any_priced = true;
        }
        let acc = self.per_model.entry(usage.model).or_default();
        add_opt(&mut acc.tokens.input, usage.tokens.input);
        add_opt(&mut acc.tokens.output, usage.tokens.output);
        add_opt(&mut acc.tokens.cache_read, usage.tokens.cache_read);
        add_opt(&mut acc.tokens.cache_write, usage.tokens.cache_write);
        if unpriced {
            acc.any_unpriced = true;
        } else {
            acc.any_priced = true;
            acc.cost = Some(acc.cost.unwrap_or(0.0) + usage.cost_total.unwrap_or(0.0));
        }
    }

    /// True when the turn made model calls and none of them has a price.
    pub fn all_unpriced(&self) -> bool {
        self.assistant_messages > 0 && !self.any_priced
    }

    /// Per-model figures, sorted by model name; `None` when no message
    /// carried usage.
    pub fn models(&self) -> Option<Vec<ModelUsage>> {
        if self.per_model.is_empty() {
            return None;
        }
        Some(
            self.per_model
                .iter()
                .map(|(model, acc)| ModelUsage {
                    model: model.clone(),
                    tokens: acc.tokens,
                    cost_usd: if acc.any_priced || !acc.any_unpriced {
                        acc.cost
                    } else {
                        None
                    },
                })
                .collect(),
        )
    }
}

fn sub_opt(delta: Option<u64>, others: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let delta = delta?;
    let sum = others.fold(0u64, |acc, n| acc.saturating_add(n.unwrap_or(0)));
    Some(delta.saturating_sub(sum))
}

/// A turn's tokens, cost and per-model figures, all from the statistics
/// `delta` (`None` is a failed reading: no data, never zero). Other models
/// keep their message sums; the main model gets the remainder, so the
/// per-model figures add up to the turn total (a single model's figures are
/// exactly the delta). This also covers usage no main-agent message carries
/// (sub-agents, side calls).
pub fn split_turn_usage(
    main_model: Option<&str>,
    messages: &TurnMessages,
    delta: Option<(TokenCounts, Option<f64>)>,
) -> (TokenCounts, Option<f64>, Option<Vec<ModelUsage>>) {
    let Some((tokens, delta_cost)) = delta else {
        return (TokenCounts::default(), None, None);
    };
    let cost_usd = if messages.all_unpriced() {
        None
    } else {
        delta_cost
    };
    let main = main_model
        .map(str::to_string)
        .or_else(|| messages.last_model.clone())
        .unwrap_or_else(|| "unknown/unknown".to_string());
    let all = messages.models().unwrap_or_default();
    let (own, others): (Vec<ModelUsage>, Vec<ModelUsage>) =
        all.into_iter().partition(|m| m.model == main);
    // A remainder below zero is possible only when a non-main model's
    // response was discarded by omp; it clamps at 0, so the per-model sums
    // then exceed the total by that amount (accepted).
    let main_tokens = TokenCounts {
        input: sub_opt(tokens.input, others.iter().map(|m| m.tokens.input)),
        output: sub_opt(tokens.output, others.iter().map(|m| m.tokens.output)),
        cache_read: sub_opt(
            tokens.cache_read,
            others.iter().map(|m| m.tokens.cache_read),
        ),
        cache_write: sub_opt(
            tokens.cache_write,
            others.iter().map(|m| m.tokens.cache_write),
        ),
    };
    // An unpriced main model is not shown as $0. In a turn mixing it with a
    // priced other model, the per-model costs then don't sum to cost_usd
    // (accepted).
    let main_unpriced = own.first().is_some_and(|m| m.cost_usd.is_none());
    let main_cost = match cost_usd {
        Some(total) if !main_unpriced => {
            let others_cost: f64 = others.iter().filter_map(|m| m.cost_usd).sum();
            Some((total - others_cost).max(0.0))
        }
        _ => None,
    };
    let mut models = others;
    models.push(ModelUsage {
        model: main,
        tokens: main_tokens,
        cost_usd: main_cost,
    });
    models.sort_by(|a, b| a.model.cmp(&b.model));
    (tokens, cost_usd, Some(models))
}

// ---------------------------------------------------------------------------
// get_session_stats
// ---------------------------------------------------------------------------

/// A `get_session_stats` reading: running totals for the omp session.
/// `reasoning` is ignored: it is already inside `output`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionStats {
    pub tokens: TokenCounts,
    pub cost: Option<f64>,
}

/// `None` when `data` isn't the documented shape (an object with a `tokens`
/// object): a garbled reading is a failed reading.
pub fn parse_session_stats(data: &Value) -> Option<SessionStats> {
    let tokens = data.get("tokens").filter(|tokens| tokens.is_object())?;
    Some(SessionStats {
        tokens: TokenCounts {
            input: u64_field(tokens, "input"),
            output: u64_field(tokens, "output"),
            cache_read: u64_field(tokens, "cacheRead"),
            cache_write: u64_field(tokens, "cacheWrite"),
        },
        cost: data.get("cost").and_then(Value::as_f64),
    })
}

fn delta_u64(now: Option<u64>, before: Option<u64>) -> Option<u64> {
    let (now, before) = (now?, before?);
    // A counter that went down restarted: take it as is.
    Some(if now >= before { now - before } else { now })
}

fn delta_f64(now: Option<f64>, before: Option<f64>) -> Option<f64> {
    let (now, before) = (now?, before?);
    Some(if now >= before { now - before } else { now })
}

/// This turn's tokens and cost: `now` minus `before`, field by field. A
/// field either reading lacks stays `None`.
pub fn stats_delta(before: &SessionStats, now: &SessionStats) -> (TokenCounts, Option<f64>) {
    (
        TokenCounts {
            input: delta_u64(now.tokens.input, before.tokens.input),
            output: delta_u64(now.tokens.output, before.tokens.output),
            cache_read: delta_u64(now.tokens.cache_read, before.tokens.cache_read),
            cache_write: delta_u64(now.tokens.cache_write, before.tokens.cache_write),
        },
        delta_f64(now.cost, before.cost),
    )
}

// ---------------------------------------------------------------------------
// Repo instruction files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// Repo-relative path, as passed to [`read_repo_instructions`].
    pub path: &'static str,
    pub contents: String,
}

/// Reads the repo's own instruction files from `cwd`, each of `names` (repo-
/// relative, in order) only if present. The agent gets nothing from parent
/// folders, `$HOME` or the operator's setup.
/// Only a regular file whose canonical path lies inside the canonical `cwd`
/// is read; one that resolves elsewhere (a symlink out of the repo) is
/// skipped with a warning message. Non-UTF-8 content is read lossily.
pub fn read_repo_instructions(
    cwd: &Path,
    names: &[&'static str],
) -> (Vec<InstructionFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let canonical_cwd = match cwd.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            warnings.push(format!(
                "couldn't resolve the working directory {}, so no repo instruction files were read: {err}",
                cwd.display()
            ));
            return (files, warnings);
        }
    };
    for &rel in names {
        let path = canonical_cwd.join(rel);
        let canonical = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                warnings.push(format!("skipped {rel}: {err}"));
                continue;
            }
        };
        if !canonical.starts_with(&canonical_cwd) {
            warnings.push(format!(
                "omp: skipped {rel}: it resolves outside the repository ({})",
                canonical.display()
            ));
            continue;
        }
        match std::fs::metadata(&canonical) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(err) => {
                warnings.push(format!("skipped {rel}: {err}"));
                continue;
            }
        }
        match std::fs::read(&canonical) {
            Ok(bytes) => files.push(InstructionFile {
                path: rel,
                contents: String::from_utf8_lossy(&bytes).into_owned(),
            }),
            Err(err) => warnings.push(format!("skipped {rel}: {err}")),
        }
    }
    (files, warnings)
}

/// Each file as `<instructions source="<path>">\n<contents>\n</instructions>\n`.
pub fn render_instruction_files(files: &[InstructionFile]) -> String {
    files
        .iter()
        .map(|file| {
            let body = file.contents.strip_suffix('\n').unwrap_or(&file.contents);
            format!(
                "<instructions source=\"{}\">\n{body}\n</instructions>\n",
                file.path
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {

    fn msg_usage(model: &str, input: u64, cost: f64) -> Option<MessageUsage> {
        Some(MessageUsage {
            model: model.to_string(),
            tokens: TokenCounts {
                input: Some(input),
                output: Some(0),
                cache_read: Some(0),
                cache_write: None,
            },
            total_tokens: input,
            cost_total: Some(cost),
        })
    }

    fn counts(i: u64, o: u64, r: u64, w: u64) -> TokenCounts {
        TokenCounts {
            input: Some(i),
            output: Some(o),
            cache_read: Some(r),
            cache_write: Some(w),
        }
    }

    fn model<'a>(models: &'a [ModelUsage], name: &str) -> &'a ModelUsage {
        models.iter().find(|m| m.model == name).unwrap()
    }

    #[test]
    fn split_one_model_is_exactly_the_delta() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("p/m", 100, 0.01));
        turn.add(msg_usage("p/m", 50, 0.01));
        let delta = (counts(300, 60, 90, 15), Some(0.0369));
        let (tokens, cost, models) = split_turn_usage(Some("p/m"), &turn, Some(delta));
        assert_eq!(tokens, delta.0);
        assert_eq!(cost, Some(0.0369));
        assert_eq!(
            models.unwrap(),
            vec![ModelUsage {
                model: "p/m".into(),
                tokens: delta.0,
                cost_usd: Some(0.0369)
            }]
        );
    }

    #[test]
    fn split_clamps_the_main_remainder_at_zero() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("q/x", 500, 0.5));
        let delta = (counts(300, 0, 0, 0), Some(0.3));
        let (_, _, models) = split_turn_usage(Some("p/m"), &turn, Some(delta));
        let models = models.unwrap();
        assert_eq!(model(&models, "p/m").tokens.input, Some(0));
        assert_eq!(model(&models, "p/m").cost_usd, Some(0.0));
        assert_eq!(model(&models, "q/x").tokens.input, Some(500));
    }

    #[test]
    fn split_keeps_none_fields_none_and_treats_other_none_as_zero() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("q/x", 100, 0.1));
        let delta = (
            TokenCounts {
                input: Some(300),
                output: None,
                cache_read: Some(0),
                cache_write: Some(15),
            },
            Some(0.3),
        );
        let (_, _, models) = split_turn_usage(Some("p/m"), &turn, Some(delta));
        let models = models.unwrap();
        let main = model(&models, "p/m");
        assert_eq!(main.tokens.output, None);
        assert_eq!(main.tokens.cache_write, Some(15));
        assert_eq!(main.tokens.input, Some(200));
    }

    #[test]
    fn split_with_no_delta_cost_has_no_cost_for_the_main_model() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("q/x", 100, 0.1));
        let (_, cost, models) =
            split_turn_usage(Some("p/m"), &turn, Some((counts(300, 0, 0, 0), None)));
        let models = models.unwrap();
        assert_eq!(cost, None);
        assert_eq!(model(&models, "p/m").cost_usd, None);
        assert_eq!(model(&models, "q/x").cost_usd, Some(0.1));
    }

    #[test]
    fn split_unpriced_other_stays_unpriced() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("q/x", 100, 0.0));
        turn.add(msg_usage("p/m", 100, 0.2));
        let (_, cost, models) =
            split_turn_usage(Some("p/m"), &turn, Some((counts(300, 0, 0, 0), Some(0.2))));
        let models = models.unwrap();
        assert_eq!(cost, Some(0.2));
        assert_eq!(model(&models, "q/x").cost_usd, None);
        assert_eq!(model(&models, "p/m").cost_usd, Some(0.2));
    }

    #[test]
    fn split_unpriced_main_with_priced_other_has_no_main_cost() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("p/m", 100, 0.0));
        turn.add(msg_usage("q/x", 100, 0.2));
        let (_, cost, models) =
            split_turn_usage(Some("p/m"), &turn, Some((counts(300, 0, 0, 0), Some(0.5))));
        let models = models.unwrap();
        assert_eq!(cost, Some(0.5));
        assert_eq!(model(&models, "p/m").cost_usd, None);
        assert_eq!(model(&models, "q/x").cost_usd, Some(0.2));
    }

    #[test]
    fn split_main_model_falls_back_to_the_last_message_then_unknown() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("q/x", 100, 0.1));
        let delta = Some((counts(100, 0, 0, 0), Some(0.1)));
        let (_, _, models) = split_turn_usage(None, &turn, delta);
        let models = models.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "q/x");
        let (_, _, models) = split_turn_usage(None, &TurnMessages::default(), delta);
        assert_eq!(models.unwrap()[0].model, "unknown/unknown");
    }

    #[test]
    fn split_gives_the_main_model_an_entry_without_messages() {
        let (_, _, models) = split_turn_usage(
            Some("p/m"),
            &TurnMessages::default(),
            Some((counts(1000, 0, 0, 0), Some(0.0))),
        );
        let models = models.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "p/m");
        assert_eq!(models[0].tokens, counts(1000, 0, 0, 0));
    }

    #[test]
    fn split_of_a_failed_reading_is_no_data() {
        let mut turn = TurnMessages::default();
        turn.add(msg_usage("p/m", 100, 0.1));
        assert_eq!(
            split_turn_usage(Some("p/m"), &turn, None),
            (TokenCounts::default(), None, None)
        );
    }
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn the_line_reader_splits_on_lf_only() {
        let text = "{\"a\":\"x\u{2028}y\"}\nsecond\r\nlast";
        let mut reader = tokio::io::BufReader::new(text.as_bytes());
        assert_eq!(
            read_lf_line(&mut reader).await.unwrap().unwrap(),
            "{\"a\":\"x\u{2028}y\"}"
        );
        assert_eq!(
            read_lf_line(&mut reader).await.unwrap().unwrap(),
            "second\r"
        );
        assert_eq!(read_lf_line(&mut reader).await.unwrap().unwrap(), "last");
        assert_eq!(read_lf_line(&mut reader).await.unwrap(), None);
    }

    #[test]
    fn messages_become_events_and_results_match_their_calls() {
        let mut n = MessageNormalizer::default();
        let events = n.normalize(
            &json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "hi"},
                {"type": "toolCall", "id": "t1", "name": "read", "arguments": {"path": "a"}},
                {"type": "toolCall", "id": "t2", "name": "report_outcome", "arguments": {}},
            ]}),
            "report_outcome",
        );
        assert_eq!(
            events,
            vec![
                AgentEvent::Thinking { text: "hmm".into() },
                AgentEvent::AssistantMessage { text: "hi".into() },
                AgentEvent::ToolCall {
                    tool_use_id: "t1".into(),
                    tool: "read".into(),
                    input: json!({"path": "a"}),
                },
            ]
        );
        let result = |id: &str, name: Option<&str>| {
            let mut m = json!({"role": "toolResult", "toolCallId": id, "isError": true,
                "content": [{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}]});
            if let Some(name) = name {
                m["toolName"] = json!(name);
            }
            m
        };
        assert_eq!(
            n.normalize(&result("t1", Some("read")), "report_outcome"),
            vec![AgentEvent::ToolResult {
                tool_use_id: "t1".into(),
                tool: "read".into(),
                output: "ab".into(),
                is_error: true,
            }]
        );
        // The name falls back to the remembered call.
        assert!(matches!(
            &n.normalize(&result("t1", None), "report_outcome")[..],
            [AgentEvent::ToolResult { tool, .. }] if tool == "read"
        ));
        assert!(
            n.normalize(&result("t2", Some("report_outcome")), "report_outcome")
                .is_empty()
        );
        assert!(
            n.normalize(&json!({"role": "user"}), "report_outcome")
                .is_empty()
        );
    }

    #[test]
    fn usage_parses_and_the_unpriced_rule_holds() {
        let msg = |cost: f64, total: u64| {
            json!({"role": "assistant", "provider": "p", "model": "m", "usage": {
                "input": 10, "output": 5, "cacheRead": 3, "cacheWrite": 1,
                "totalTokens": total, "cost": {"total": cost}}})
        };
        let u = parse_message_usage(&msg(0.5, 19)).unwrap();
        assert_eq!(u.model, "p/m");
        assert_eq!(u.tokens.cache_read, Some(3));
        assert!(!u.unpriced());
        assert!(parse_message_usage(&msg(0.0, 19)).unwrap().unpriced());
        assert!(!parse_message_usage(&msg(0.0, 0)).unwrap().unpriced());
        assert!(parse_message_usage(&json!({"role": "assistant"})).is_none());

        let mut turn = TurnMessages::default();
        assert!(!turn.all_unpriced(), "no model calls is not unpriced");
        turn.add(parse_message_usage(&msg(0.0, 19)));
        assert!(turn.all_unpriced());
        assert_eq!(turn.models().unwrap()[0].cost_usd, None);
        turn.add(parse_message_usage(&msg(0.5, 19)));
        assert!(!turn.all_unpriced());
        let models = turn.models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].tokens.input, Some(20));
        assert_eq!(models[0].cost_usd, Some(0.5));
        assert_eq!(turn.assistant_messages, 2);
    }

    #[test]
    fn stats_parse_and_delta() {
        let stats = |input: u64, cost: f64| {
            parse_session_stats(&json!({"tokens": {"input": input, "output": 4,
                "reasoning": 2, "cacheRead": 1, "cacheWrite": 0, "total": 9}, "cost": cost}))
            .unwrap()
        };
        let (tokens, cost) = stats_delta(&stats(10, 1.0), &stats(25, 1.5));
        assert_eq!(tokens.input, Some(15));
        assert_eq!(tokens.output, Some(0));
        assert_eq!(cost, Some(0.5));
        // Lower than the baseline: taken as is.
        let (tokens, cost) = stats_delta(&stats(10, 1.0), &stats(3, 0.25));
        assert_eq!((tokens.input, cost), (Some(3), Some(0.25)));
        assert!(parse_session_stats(&json!({"cost": 1})).is_none());
        assert!(parse_session_stats(&json!("nope")).is_none());
        let partial = parse_session_stats(&json!({"tokens": {"input": 1}})).unwrap();
        let (tokens, cost) = stats_delta(&partial, &partial);
        assert_eq!((tokens.input, tokens.output, cost), (Some(0), None, None));
    }

    #[test]
    fn instruction_files_are_read_in_order_and_symlinks_out_are_skipped() {
        let dir = tempdir("pi-family-instr");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".omp")).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "agents\n").unwrap();
        std::fs::write(repo.join("CLAUDE.md"), [b'c', 0xff]).unwrap();
        std::fs::write(repo.join(".omp/RULES.md"), "rules").unwrap();
        std::fs::write(repo.join(".omp/mcp.json"), "{}").unwrap();
        std::fs::write(dir.join("outside.md"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.md"), repo.join(".omp/AGENTS.md")).unwrap();
        let (files, warnings) = read_repo_instructions(
            &repo,
            &["CLAUDE.md", "AGENTS.md", ".omp/AGENTS.md", ".omp/RULES.md"],
        );
        let paths: Vec<_> = files.iter().map(|f| f.path).collect();
        assert_eq!(paths, vec!["CLAUDE.md", "AGENTS.md", ".omp/RULES.md"]);
        assert_eq!(files[0].contents, "c\u{fffd}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(".omp/AGENTS.md"), "{warnings:?}");
        let rendered = render_instruction_files(&files);
        assert!(
            rendered.contains("<instructions source=\".omp/RULES.md\">\nrules\n</instructions>\n")
        );
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("{}"));
        // An unresolvable directory reads nothing and says so.
        let (files, warnings) = read_repo_instructions(&dir.join("missing"), &["CLAUDE.md"]);
        assert!(files.is_empty() && warnings.len() == 1);
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }
}
