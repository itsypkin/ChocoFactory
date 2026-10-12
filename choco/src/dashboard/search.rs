//! Task search for the dashboard's `/` popup (#292). Pure: no terminal or
//! app types, so `choco task list --search` (#293) can reuse it.

use chocofactory_core::models::TaskSummary;

/// Tasks matching `query`, newest first (`created_at` descending, ties by id
/// ascending). The rules apply to the trimmed query:
///
/// 1. empty: every task;
/// 2. `#` and digits: the task whose PR number is exactly that;
/// 3. digits only: PR number exactly that, or an id starting with it;
/// 4. anything else: title contains it, or id starts with it (any case).
pub fn search<'a>(tasks: &'a [TaskSummary], query: &str) -> Vec<&'a TaskSummary> {
    let q = query.trim();
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let pr_is = |t: &TaskSummary, digits: &str| {
        t.pr.as_ref()
            .is_some_and(|pr| pr.number.to_string() == digits)
    };
    let lower = q.to_lowercase();
    let mut out: Vec<&TaskSummary> = tasks
        .iter()
        .filter(|t| {
            if q.is_empty() {
                true
            } else if let Some(digits) = q.strip_prefix('#').filter(|d| all_digits(d)) {
                pr_is(t, digits)
            } else if all_digits(q) {
                pr_is(t, q) || t.task.id.to_lowercase().starts_with(&lower)
            } else {
                t.task.title.to_lowercase().contains(&lower)
                    || t.task.id.to_lowercase().starts_with(&lower)
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.task
            .created_at
            .cmp(&a.task.created_at)
            .then_with(|| a.task.id.cmp(&b.task.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chocofactory_core::models::{PullRequestRef, Task};
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    fn t(id: &str, title: &str, pr: Option<u64>, created_min: u32) -> TaskSummary {
        TaskSummary {
            queued: Vec::new(),
            task: Task {
                id: id.to_string(),
                project_id: "p".to_string(),
                workflow_def: "w".to_string(),
                title: title.to_string(),
                status: "closed".to_string(),
                config: json!({}),
                worktree_repo: None,
                worktree_project: None,
                stuck_reason: None,
                paused_at: None,
                kept_work: false,
                workflow_path: None,
                workflow_sha256: None,
                base_ref: None,
                base_commit: None,
                closes: vec![],
                created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, created_min, 0).unwrap(),
                updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            },
            current_stage: None,
            stage_entered_at: None,
            loop_counters: json!({}),
            pr: pr.map(|number| PullRequestRef {
                number,
                url: format!("https://x/pull/{number}"),
            }),
            waiting_on_human: false,
            usage_total: None,
            branch_progress: None,
        }
    }

    fn ids(v: &[&TaskSummary]) -> Vec<String> {
        v.iter().map(|t| t.task.id.clone()).collect()
    }

    fn fixture() -> Vec<TaskSummary> {
        vec![
            t("aaaa1111", "Fix the parser", Some(289), 1),
            t("bbbb2222", "Other", Some(28), 2),
            t("cccc3333-x", "Add --closes flag", Some(2890), 3),
            t("12ab0000", "Mentions 289 in title", None, 4),
            t("dddd4444", "No pr here", None, 5),
        ]
    }

    #[test]
    fn pr_number_is_exact_with_and_without_hash() {
        let f = fixture();
        assert_eq!(ids(&search(&f, "#289")), ["aaaa1111"]);
        assert_eq!(ids(&search(&f, "289")), ["aaaa1111"]);
        assert_eq!(ids(&search(&f, " #289 ")), ["aaaa1111"]);
    }

    #[test]
    fn digits_match_id_prefix_but_hash_digits_do_not() {
        let f = fixture();
        assert_eq!(ids(&search(&f, "12")), ["12ab0000"]);
        assert!(search(&f, "#12").is_empty());
    }

    #[test]
    fn title_is_case_insensitive_substring_and_trimmed() {
        let f = fixture();
        assert_eq!(ids(&search(&f, "CLOSES")), ["cccc3333-x"]);
        assert_eq!(ids(&search(&f, "  closes  ")), ["cccc3333-x"]);
    }

    #[test]
    fn id_prefix_with_letters_any_case() {
        let f = fixture();
        assert_eq!(ids(&search(&f, "cccc3")), ["cccc3333-x"]);
        assert_eq!(ids(&search(&f, "CCCC3")), ["cccc3333-x"]);
    }

    #[test]
    fn empty_and_blank_queries_return_all_and_no_match_is_empty() {
        let f = fixture();
        assert_eq!(search(&f, "").len(), 5);
        assert_eq!(search(&f, "   ").len(), 5);
        assert!(search(&f, "zzzz").is_empty());
    }

    #[test]
    fn newest_first_with_id_tiebreak() {
        let f = vec![
            t("b", "x", None, 1),
            t("z", "x", None, 9),
            t("a", "x", None, 5),
            t("c", "x", None, 5),
        ];
        assert_eq!(ids(&search(&f, "")), ["z", "a", "c", "b"]);
    }

    #[test]
    fn lone_hash_and_hash_text_follow_the_text_rule() {
        let f = vec![
            t("a1", "Fix #abc handling", Some(5), 1),
            t("b1", "C# support", Some(6), 2),
            t("c1", "plain", Some(7), 3),
        ];
        assert_eq!(ids(&search(&f, "#")), ["b1", "a1"]);
        assert_eq!(ids(&search(&f, "#abc")), ["a1"]);
    }
}
