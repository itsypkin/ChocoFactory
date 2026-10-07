//! Per-turn usage: turning the CLI's running totals into per-turn values,
//! and rolling stored turns up into a task's cost & time figures. Both are
//! pure functions of their inputs so they can be tested without a database.

use std::collections::BTreeMap;

use chocofactory_core::models::Event;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::adapter::{TokenCounts, TurnUsage, UsageCounting};

/// One model's figures in a turn, as stored in `turn_usage.models` /
/// `reported_models` (a JSON object keyed by model name).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelFigures {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

pub type ModelMap = BTreeMap<String, ModelFigures>;

/// The reported figures the next turn is measured against: the latest
/// non-null value of each, found by the caller by walking the session and
/// its `resumed_from` chain. `None` = none found (counts as 0).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Baseline {
    pub cost_usd: Option<f64>,
    pub models: Option<ModelMap>,
}

/// A turn's own cost and per-model figures.
#[derive(Debug, Clone, PartialEq)]
pub struct PerTurn {
    pub cost_usd: Option<f64>,
    pub models: Option<ModelMap>,
    /// The per-model figures as reported (kept on the row for audit).
    pub reported_models: Option<ModelMap>,
}

pub fn reported_models(usage: &TurnUsage) -> Option<ModelMap> {
    usage.models.as_ref().map(|models| {
        models
            .iter()
            .map(|m| {
                (
                    m.model.clone(),
                    ModelFigures {
                        input_tokens: m.tokens.input,
                        output_tokens: m.tokens.output,
                        cache_read_tokens: m.tokens.cache_read,
                        cache_write_tokens: m.tokens.cache_write,
                        cost_usd: m.cost_usd,
                    },
                )
            })
            .collect()
    })
}

fn delta_f64(reported: f64, prev: f64) -> f64 {
    if reported >= prev {
        reported - prev
    } else {
        // The counter restarted.
        reported
    }
}

fn delta_u64(reported: Option<u64>, prev: Option<u64>) -> Option<u64> {
    let reported = reported?;
    let prev = prev.unwrap_or(0);
    Some(if reported >= prev {
        reported - prev
    } else {
        reported
    })
}

/// Converts what a turn reported into the turn's own figures.
///
/// `PerTurn` counting is taken as reported. Cumulative cost is the reported
/// value minus the baseline (0 when there is none); a value below the
/// baseline means the counter restarted, so it is taken as is. Per-model
/// figures follow the same rule, per model and per field, with a model or
/// field missing from the baseline counting as 0. A `None` stays `None`.
pub fn per_turn(usage: &TurnUsage, baseline: &Baseline) -> PerTurn {
    let reported = reported_models(usage);
    match usage.counting {
        UsageCounting::PerTurn => PerTurn {
            cost_usd: usage.cost_usd,
            models: reported.clone(),
            reported_models: reported,
        },
        UsageCounting::CumulativePerConversation => {
            let cost_usd = usage
                .cost_usd
                .map(|c| delta_f64(c, baseline.cost_usd.unwrap_or(0.0)));
            let empty = ModelMap::new();
            let base = baseline.models.as_ref().unwrap_or(&empty);
            let models = reported.as_ref().map(|models| {
                models
                    .iter()
                    .map(|(name, now)| {
                        let prev = base.get(name).copied().unwrap_or_default();
                        (
                            name.clone(),
                            ModelFigures {
                                input_tokens: delta_u64(now.input_tokens, prev.input_tokens),
                                output_tokens: delta_u64(now.output_tokens, prev.output_tokens),
                                cache_read_tokens: delta_u64(
                                    now.cache_read_tokens,
                                    prev.cache_read_tokens,
                                ),
                                cache_write_tokens: delta_u64(
                                    now.cache_write_tokens,
                                    prev.cache_write_tokens,
                                ),
                                cost_usd: now
                                    .cost_usd
                                    .map(|c| delta_f64(c, prev.cost_usd.unwrap_or(0.0))),
                            },
                        )
                    })
                    .collect()
            });
            PerTurn {
                cost_usd,
                models,
                reported_models: reported,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// A session's facts the roll-up groups by.
#[derive(Debug, Clone)]
pub struct SessionFacts {
    pub id: String,
    pub stage: String,
    pub role: String,
    pub lap: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub ended: bool,
}

/// One stored `turn_usage` row, with its per-turn values.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub session_id: String,
    pub billing: String,
    pub cost_usd: Option<f64>,
    pub tokens: TokenCounts,
    pub models: Option<ModelMap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TokenTotals {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageUsage {
    pub stage: String,
    pub cost_usd: Option<f64>,
    pub tokens: Option<TokenTotals>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoleUsage {
    pub role: String,
    pub cost_usd: Option<f64>,
    pub tokens: Option<TokenTotals>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LapUsage {
    pub stage: String,
    pub lap: Option<i64>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<TokenTotals>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelTotals {
    pub model: String,
    pub cost_usd: Option<f64>,
    pub tokens: TokenTotals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskUsage {
    pub cost_usd: Option<f64>,
    pub billing_label: &'static str,
    pub tokens: TokenTotals,
    pub wall_time_ms: i64,
    pub active_time_ms: Option<i64>,
    pub sessions_without_data: usize,
    pub by_stage: Vec<StageUsage>,
    pub by_role: Vec<RoleUsage>,
    pub by_lap: Vec<LapUsage>,
    pub by_model: Vec<ModelTotals>,
}

/// What the roll-up needs to know about the task itself.
#[derive(Debug, Clone, Copy)]
pub struct TaskTimes<'a> {
    pub status: &'a str,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Default, Clone, Copy)]
struct Sums {
    cost: Option<f64>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

fn add_f(acc: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0.0) + v);
    }
}

fn add_u(acc: &mut Option<u64>, v: Option<u64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0) + v);
    }
}

impl Sums {
    fn add(&mut self, cost: Option<f64>, tokens: &TokenCounts) {
        add_f(&mut self.cost, cost);
        add_u(&mut self.input, tokens.input);
        add_u(&mut self.output, tokens.output);
        add_u(&mut self.cache_read, tokens.cache_read);
        add_u(&mut self.cache_write, tokens.cache_write);
    }

    fn totals(&self) -> TokenTotals {
        TokenTotals {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        }
    }
}

/// A group of sessions: the earliest start, and the sums of its rows
/// (`None` while none of its sessions has a row).
struct Group {
    first_started: DateTime<Utc>,
    sums: Option<Sums>,
}

fn grouped<K: Ord + Clone>(
    sessions: &[SessionFacts],
    rows: &[UsageRow],
    key: impl Fn(&SessionFacts) -> K,
) -> Vec<(K, Option<Sums>)> {
    let mut groups: BTreeMap<K, Group> = BTreeMap::new();
    for s in sessions {
        let g = groups.entry(key(s)).or_insert(Group {
            first_started: s.started_at,
            sums: None,
        });
        g.first_started = g.first_started.min(s.started_at);
    }
    for row in rows {
        let Some(session) = sessions.iter().find(|s| s.id == row.session_id) else {
            continue;
        };
        if let Some(g) = groups.get_mut(&key(session)) {
            g.sums
                .get_or_insert_with(Sums::default)
                .add(row.cost_usd, &row.tokens);
        }
    }
    let mut out: Vec<(K, Group)> = groups.into_iter().collect();
    out.sort_by(|a, b| a.1.first_started.cmp(&b.1.first_started));
    out.into_iter().map(|(k, g)| (k, g.sums)).collect()
}

fn clamp_ms(from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
    (to - from).num_milliseconds().max(0)
}

/// Active time from the stage trail: visits from one non-`retry` entry to
/// the next, leaving out `human_gate` and `terminal` stages. An entry with
/// no `kind` (written before kinds were recorded) counts.
fn active_time_ms(trail: &[Event], end: DateTime<Utc>) -> Option<i64> {
    if trail.is_empty() {
        return None;
    }
    let entries: Vec<&Event> = trail
        .iter()
        .filter(|e| e.payload.get("outcome").and_then(|o| o.as_str()) != Some("retry"))
        .collect();
    let mut total = 0;
    for (i, entry) in entries.iter().enumerate() {
        let until = entries.get(i + 1).map_or(end, |next| next.created_at);
        let kind = entry.payload.get("kind").and_then(|k| k.as_str());
        if matches!(kind, Some("human_gate" | "terminal")) {
            continue;
        }
        total += clamp_ms(entry.created_at, until);
    }
    Some(total)
}

/// Rolls a task's stored turns up. `None` when the task has no rows.
pub fn aggregate(
    task: TaskTimes<'_>,
    sessions: &[SessionFacts],
    rows: &[UsageRow],
    trail: &[Event],
    now: DateTime<Utc>,
) -> Option<TaskUsage> {
    if rows.is_empty() {
        return None;
    }
    let mut total = Sums::default();
    for row in rows {
        total.add(row.cost_usd, &row.tokens);
    }
    let billing_label = if rows.iter().all(|r| r.billing == "subscription") {
        "api_equivalent"
    } else {
        "estimated"
    };
    let open = task.status == "open";
    let live = open || task.status == "stuck";
    let wall_time_ms = clamp_ms(task.created_at, if live { now } else { task.updated_at });
    let active_time_ms = active_time_ms(trail, if open { now } else { task.updated_at });
    let sessions_without_data = sessions
        .iter()
        .filter(|s| s.ended && !rows.iter().any(|r| r.session_id == s.id))
        .count();

    let split = |sums: Option<Sums>| match sums {
        Some(s) => (s.cost, Some(s.totals())),
        None => (None, None),
    };
    let by_stage = grouped(sessions, rows, |s| s.stage.clone())
        .into_iter()
        .map(|(stage, sums)| {
            let (cost_usd, tokens) = split(sums);
            StageUsage {
                stage,
                cost_usd,
                tokens,
            }
        })
        .collect();
    let by_role = grouped(sessions, rows, |s| s.role.clone())
        .into_iter()
        .map(|(role, sums)| {
            let (cost_usd, tokens) = split(sums);
            RoleUsage {
                role,
                cost_usd,
                tokens,
            }
        })
        .collect();
    let by_lap = grouped(sessions, rows, |s| (s.stage.clone(), s.lap))
        .into_iter()
        .map(|((stage, lap), sums)| {
            let (cost_usd, tokens) = split(sums);
            LapUsage {
                stage,
                lap,
                cost_usd,
                tokens,
            }
        })
        .collect();

    let mut models: BTreeMap<String, Sums> = BTreeMap::new();
    for row in rows {
        for (name, m) in row.models.iter().flatten() {
            models.entry(name.clone()).or_default().add(
                m.cost_usd,
                &TokenCounts {
                    input: m.input_tokens,
                    output: m.output_tokens,
                    cache_read: m.cache_read_tokens,
                    cache_write: m.cache_write_tokens,
                },
            );
        }
    }
    let mut by_model: Vec<ModelTotals> = models
        .into_iter()
        .map(|(model, s)| ModelTotals {
            model,
            cost_usd: s.cost,
            tokens: s.totals(),
        })
        .collect();
    by_model.sort_by(|a, b| {
        b.cost_usd
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&a.cost_usd.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| a.model.cmp(&b.model))
    });

    Some(TaskUsage {
        cost_usd: total.cost,
        billing_label,
        tokens: total.totals(),
        wall_time_ms,
        active_time_ms,
        sessions_without_data,
        by_stage,
        by_role,
        by_lap,
        by_model,
    })
}
