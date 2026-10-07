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

/// A turn's token counts. `result.usage` holds only the main model's
/// tokens, while the per-model figures also cover sub-agent models, so when
/// the CLI reports per-model figures a kind's count is their sum. A kind no
/// model reports keeps the CLI's own `usage` figure.
pub fn turn_tokens(reported: &TokenCounts, models: Option<&ModelMap>) -> TokenCounts {
    let sum = |pick: fn(&ModelFigures) -> Option<u64>, fallback: Option<u64>| {
        let mut acc: Option<u64> = None;
        for m in models.into_iter().flat_map(|m| m.values()) {
            add_u(&mut acc, pick(m));
        }
        acc.or(fallback)
    };
    TokenCounts {
        input: sum(|m| m.input_tokens, reported.input),
        output: sum(|m| m.output_tokens, reported.output),
        cache_read: sum(|m| m.cache_read_tokens, reported.cache_read),
        cache_write: sum(|m| m.cache_write_tokens, reported.cache_write),
    }
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
    /// Turns whose cost is unknown: the total leaves them out.
    pub turns_without_cost: usize,
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
    out.sort_by_key(|a| a.1.first_started);
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
        .filter(|e| {
            !matches!(
                e.payload.get("outcome").and_then(|o| o.as_str()),
                Some("retry" | "retry_resume")
            )
        })
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
    let turns_without_cost = rows.iter().filter(|r| r.cost_usd.is_none()).count();
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
        turns_without_cost,
        by_stage,
        by_role,
        by_lap,
        by_model,
    })
}

#[cfg(test)]
mod tests {
    use chocofactory_core::models::EventType;
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::adapter::{BillingMode, ModelUsage};

    fn tokens(n: Option<u64>) -> TokenCounts {
        TokenCounts {
            input: n,
            output: n,
            cache_read: n,
            cache_write: n,
        }
    }

    fn usage(
        cost: Option<f64>,
        input: Option<u64>,
        counting: UsageCounting,
        models: Option<Vec<ModelUsage>>,
    ) -> TurnUsage {
        TurnUsage {
            cost_usd: cost,
            tokens: tokens(input),
            models,
            wall_time_ms: None,
            model_turns: None,
            billing: BillingMode::Subscription,
            counting,
        }
    }

    fn cumulative(cost: Option<f64>) -> TurnUsage {
        usage(
            cost,
            Some(10),
            UsageCounting::CumulativePerConversation,
            None,
        )
    }

    fn model(name: &str, input: u64, cost: f64) -> ModelUsage {
        ModelUsage {
            model: name.to_string(),
            tokens: tokens(Some(input)),
            cost_usd: Some(cost),
        }
    }

    fn base_cost(c: f64) -> Baseline {
        Baseline {
            cost_usd: Some(c),
            models: None,
        }
    }

    #[test]
    fn two_turns_in_one_session_give_each_turns_own_cost() {
        let first = per_turn(&cumulative(Some(0.02490)), &Baseline::default());
        assert!((first.cost_usd.unwrap() - 0.02490).abs() < 1e-9);
        let second = per_turn(&cumulative(Some(0.02927)), &base_cost(0.02490));
        assert!((second.cost_usd.unwrap() - 0.00437).abs() < 1e-9);
    }

    #[test]
    fn a_resumed_sessions_first_turn_continues_the_chain() {
        let third = per_turn(&cumulative(Some(0.03288)), &base_cost(0.02927));
        assert!((third.cost_usd.unwrap() - 0.00361).abs() < 1e-9);
    }

    #[test]
    fn a_counter_restart_takes_the_reported_value() {
        let t = per_turn(&cumulative(Some(0.01)), &base_cost(0.03));
        assert_eq!(t.cost_usd, Some(0.01));
    }

    #[test]
    fn a_fresh_session_starts_from_zero() {
        let t = per_turn(&cumulative(Some(0.02490)), &Baseline::default());
        assert_eq!(t.cost_usd, Some(0.02490));
    }

    #[test]
    fn a_turn_without_cost_has_no_per_turn_cost() {
        let t = per_turn(&cumulative(None), &base_cost(0.5));
        assert_eq!(t.cost_usd, None);
    }

    #[test]
    fn per_turn_counting_is_taken_as_reported() {
        let u = usage(
            Some(0.5),
            Some(1),
            UsageCounting::PerTurn,
            Some(vec![model("m", 7, 0.5)]),
        );
        let t = per_turn(&u, &base_cost(0.4));
        assert_eq!(t.cost_usd, Some(0.5));
        assert_eq!(t.models.unwrap()["m"].input_tokens, Some(7));
    }

    fn cumulative_models(models: Vec<ModelUsage>) -> TurnUsage {
        usage(
            None,
            None,
            UsageCounting::CumulativePerConversation,
            Some(models),
        )
    }

    #[test]
    fn per_model_figures_are_differenced_per_field() {
        let mut baseline = Baseline::default();
        let mut got = Vec::new();
        for (input, cost) in [(10, 0.02), (20, 0.04), (30, 0.06)] {
            let u = cumulative_models(vec![model("m", input, cost)]);
            let t = per_turn(&u, &baseline);
            let m = t.models.as_ref().unwrap()["m"];
            got.push((m.input_tokens.unwrap(), m.cost_usd.unwrap()));
            baseline.models = t.reported_models;
        }
        for (input, cost) in got {
            assert_eq!(input, 10);
            assert!((cost - 0.02).abs() < 1e-9);
        }
    }

    #[test]
    fn a_model_absent_from_the_baseline_keeps_its_full_figures() {
        let baseline = Baseline {
            cost_usd: None,
            models: Some(ModelMap::from([(
                "old".to_string(),
                ModelFigures {
                    input_tokens: Some(5),
                    ..Default::default()
                },
            )])),
        };
        let t = per_turn(&cumulative_models(vec![model("new", 9, 0.3)]), &baseline);
        let m = t.models.unwrap()["new"];
        assert_eq!(m.input_tokens, Some(9));
        assert_eq!(m.cost_usd, Some(0.3));
    }

    #[test]
    fn a_missing_per_model_field_stays_none() {
        let mut m = model("m", 9, 0.3);
        m.tokens.output = None;
        let t = per_turn(&cumulative_models(vec![m]), &Baseline::default());
        assert_eq!(t.models.unwrap()["m"].output_tokens, None);
    }

    // ---- aggregation ----

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn session(
        id: &str,
        stage: &str,
        role: &str,
        lap: Option<i64>,
        start: i64,
        ended: bool,
    ) -> SessionFacts {
        SessionFacts {
            id: id.to_string(),
            stage: stage.to_string(),
            role: role.to_string(),
            lap,
            started_at: at(start),
            ended,
        }
    }

    fn row(
        session_id: &str,
        billing: &str,
        cost: Option<f64>,
        n: u64,
        models: Option<ModelMap>,
    ) -> UsageRow {
        UsageRow {
            session_id: session_id.to_string(),
            billing: billing.to_string(),
            cost_usd: cost,
            tokens: tokens(Some(n)),
            models,
        }
    }

    fn entry(secs: i64, outcome: Option<&str>, kind: Option<&str>) -> Event {
        let mut payload = json!({ "stage": "s", "outcome": outcome });
        if let Some(kind) = kind {
            payload["kind"] = json!(kind);
        }
        Event {
            id: format!("e{secs}"),
            task_id: "t".to_string(),
            session_id: None,
            event_type: EventType::StageEntered,
            payload,
            created_at: at(secs),
        }
    }

    fn times(status: &str) -> TaskTimes<'_> {
        TaskTimes {
            status,
            created_at: at(0),
            updated_at: at(500),
        }
    }

    fn fixture() -> (Vec<SessionFacts>, Vec<UsageRow>) {
        let sessions = vec![
            session("a", "implement", "coder", Some(1), 10, true),
            session("b", "review", "reviewer", Some(1), 20, true),
            session("c", "implement", "coder", Some(2), 30, true),
            // resumed from c: same lap
            session("d", "implement", "coder", Some(2), 40, true),
            // ended without a result line
            session("e", "review", "reviewer", Some(2), 50, true),
        ];
        let m = |cost: f64, n: u64| {
            Some(ModelMap::from([(
                "mock".to_string(),
                ModelFigures {
                    input_tokens: Some(n),
                    cost_usd: Some(cost),
                    ..Default::default()
                },
            )]))
        };
        let rows = vec![
            row("a", "subscription", Some(0.01), 1, m(0.01, 1)),
            row("b", "subscription", Some(0.02), 2, m(0.02, 2)),
            row("c", "subscription", Some(0.03), 3, m(0.03, 3)),
            row("d", "subscription", Some(0.04), 4, m(0.04, 4)),
        ];
        (sessions, rows)
    }

    #[test]
    fn totals_and_breakdowns_group_by_stage_role_lap_and_model() {
        let (sessions, rows) = fixture();
        let u = aggregate(times("closed"), &sessions, &rows, &[], at(900)).unwrap();
        assert!((u.cost_usd.unwrap() - 0.10).abs() < 1e-9);
        assert_eq!(u.tokens.input, Some(10));
        assert_eq!(u.billing_label, "api_equivalent");
        assert_eq!(u.sessions_without_data, 1);

        assert_eq!(
            u.by_stage
                .iter()
                .map(|s| s.stage.as_str())
                .collect::<Vec<_>>(),
            ["implement", "review"]
        );
        assert!((u.by_stage[0].cost_usd.unwrap() - 0.08).abs() < 1e-9);
        assert_eq!(u.by_stage[0].tokens.unwrap().input, Some(8));
        assert!((u.by_stage[1].cost_usd.unwrap() - 0.02).abs() < 1e-9);
        assert_eq!(
            u.by_role
                .iter()
                .map(|s| s.role.as_str())
                .collect::<Vec<_>>(),
            ["coder", "reviewer"]
        );

        let laps: Vec<_> = u.by_lap.iter().map(|l| (l.stage.as_str(), l.lap)).collect();
        assert_eq!(
            laps,
            [
                ("implement", Some(1)),
                ("review", Some(1)),
                ("implement", Some(2)),
                ("review", Some(2))
            ]
        );
        // Session d resumed c, so lap 2 of implement holds both.
        assert!((u.by_lap[2].cost_usd.unwrap() - 0.07).abs() < 1e-9);
        // Lap 2 of review: only a session with no rows.
        assert_eq!(u.by_lap[3].cost_usd, None);
        assert_eq!(u.by_lap[3].tokens, None);

        assert_eq!(u.by_model.len(), 1);
        assert_eq!(u.by_model[0].model, "mock");
        assert!((u.by_model[0].cost_usd.unwrap() - 0.10).abs() < 1e-9);
        assert_eq!(u.by_model[0].tokens.input, Some(10));
    }

    #[test]
    fn a_group_whose_sessions_have_no_rows_shows_null() {
        let (sessions, rows) = fixture();
        let rows: Vec<_> = rows.into_iter().filter(|r| r.session_id != "b").collect();
        let u = aggregate(times("closed"), &sessions, &rows, &[], at(900)).unwrap();
        // review still has session e without rows, and b has none now.
        let review = u.by_stage.iter().find(|s| s.stage == "review").unwrap();
        assert_eq!(review.cost_usd, None);
        assert_eq!(review.tokens, None);
        assert_eq!(u.sessions_without_data, 2);
    }

    #[test]
    fn by_model_is_ordered_by_cost_highest_first() {
        let sessions = vec![session("a", "s", "r", Some(1), 0, false)];
        let fig = |c: f64| ModelFigures {
            cost_usd: Some(c),
            ..Default::default()
        };
        let rows = vec![row(
            "a",
            "subscription",
            Some(0.3),
            1,
            Some(ModelMap::from([
                ("cheap".to_string(), fig(0.1)),
                ("dear".to_string(), fig(0.2)),
            ])),
        )];
        let u = aggregate(times("open"), &sessions, &rows, &[], at(10)).unwrap();
        assert_eq!(u.by_model[0].model, "dear");
        assert_eq!(u.by_model[1].model, "cheap");
    }

    #[test]
    fn no_rows_means_no_usage_not_zero() {
        let (sessions, _) = fixture();
        assert!(aggregate(times("open"), &sessions, &[], &[], at(1)).is_none());
    }

    #[test]
    fn the_billing_label_is_api_equivalent_only_when_every_row_is_subscription() {
        let sessions = vec![session("a", "s", "r", Some(1), 0, false)];
        let label = |billings: &[&str]| {
            let rows: Vec<_> = billings
                .iter()
                .map(|b| row("a", b, Some(0.1), 1, None))
                .collect();
            aggregate(times("open"), &sessions, &rows, &[], at(1))
                .unwrap()
                .billing_label
        };
        assert_eq!(label(&["subscription", "subscription"]), "api_equivalent");
        assert_eq!(label(&["subscription", "api_key"]), "estimated");
        assert_eq!(label(&["subscription", "unknown"]), "estimated");
    }

    #[test]
    fn unknown_costs_and_tokens_total_to_null() {
        let sessions = vec![session("a", "s", "r", Some(1), 0, false)];
        let mut r = row("a", "unknown", None, 0, None);
        r.tokens = tokens(None);
        let u = aggregate(times("open"), &sessions, &[r], &[], at(1)).unwrap();
        assert_eq!(u.cost_usd, None);
        assert_eq!(u.tokens.input, None);
    }

    #[test]
    fn a_turn_with_tokens_but_no_cost_is_counted_as_missing_from_the_total() {
        let sessions = vec![session("a", "s", "r", Some(1), 0, false)];
        let rows = [
            row("a", "subscription", Some(0.5), 10, None),
            row("a", "subscription", None, 10, None),
        ];
        let u = aggregate(times("open"), &sessions, &rows, &[], at(1)).unwrap();
        assert_eq!(u.cost_usd, Some(0.5));
        assert_eq!(u.turns_without_cost, 1);
        assert_eq!(u.tokens.input, Some(20));
    }

    #[test]
    fn wall_time_runs_to_now_while_open_or_stuck_and_to_updated_at_after() {
        let (sessions, rows) = fixture();
        let wall = |status: &str| {
            aggregate(times(status), &sessions, &rows, &[], at(900))
                .unwrap()
                .wall_time_ms
        };
        assert_eq!(wall("open"), 900_000);
        assert_eq!(wall("stuck"), 900_000);
        assert_eq!(wall("closed"), 500_000);
        assert_eq!(wall("cancelled"), 500_000);
    }

    #[test]
    fn active_time_leaves_out_gates_and_retries_and_counts_unlabelled_entries() {
        let (sessions, rows) = fixture();
        // Visits (a retry entry does not split one):
        //   [0, 100)   agent_turn            counts: 100
        //   [100, 300) human_gate            left out
        //   [300, 400) no kind (old entry)   counts: 100
        //   [400, 500) agent_turn, to updated_at: 100
        let trail = vec![
            entry(0, None, Some("agent_turn")),
            entry(100, Some("go"), Some("human_gate")),
            entry(300, Some("approved"), None),
            entry(340, Some("retry"), Some("human_gate")),
            entry(350, Some("retry_resume"), Some("human_gate")),
            entry(400, Some("again"), Some("agent_turn")),
        ];
        let u = aggregate(times("closed"), &sessions, &rows, &trail, at(900)).unwrap();
        assert_eq!(u.active_time_ms, Some(300_000));
    }

    #[test]
    fn active_time_ends_at_now_for_an_open_task_and_is_null_for_an_empty_trail() {
        let (sessions, rows) = fixture();
        let trail = vec![entry(0, None, Some("agent_turn"))];
        let u = aggregate(times("open"), &sessions, &rows, &trail, at(900)).unwrap();
        assert_eq!(u.active_time_ms, Some(900_000));
        let u = aggregate(times("open"), &sessions, &rows, &[], at(900)).unwrap();
        assert_eq!(u.active_time_ms, None);
    }

    #[test]
    fn a_terminal_visit_is_not_active() {
        let (sessions, rows) = fixture();
        let trail = vec![
            entry(0, None, Some("agent_turn")),
            entry(100, Some("done"), Some("terminal")),
        ];
        let u = aggregate(times("closed"), &sessions, &rows, &trail, at(900)).unwrap();
        assert_eq!(u.active_time_ms, Some(100_000));
    }
}
