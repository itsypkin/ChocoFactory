//! Task matching shared by the dashboard's `/` popup and
//! `choco task list --search`. Pure: no terminal or app types.

use chocofactory_core::models::TaskSummary;

/// Whether `task` matches `query`. The rules apply to the trimmed query:
///
/// 1. empty: every task;
/// 2. `#` or `!` and digits: the task whose review number is exactly that;
/// 3. digits only: review number exactly that, or an id starting with it;
/// 4. anything else: title contains it, or id starts with it (any case).
///
/// The review's label and URL are never matched. Digits are compared as
/// typed (`042` is not review 42).
pub fn matches(task: &TaskSummary, query: &str) -> bool {
    let q = query.trim();
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let review_is = |t: &TaskSummary, digits: &str| {
        t.reviews
            .first()
            .is_some_and(|review| review.number.to_string() == digits)
    };
    let lower = q.to_lowercase();
    if q.is_empty() {
        true
    } else if let Some(digits) = q.strip_prefix(['#', '!']).filter(|d| all_digits(d)) {
        review_is(task, digits)
    } else if all_digits(q) {
        review_is(task, q) || task.task.id.to_lowercase().starts_with(&lower)
    } else {
        task.task.title.to_lowercase().contains(&lower)
            || task.task.id.to_lowercase().starts_with(&lower)
    }
}

/// Tasks that `matches` the query, newest first (`created_at` descending,
/// ties by id ascending).
pub fn search<'a>(tasks: &'a [TaskSummary], query: &str) -> Vec<&'a TaskSummary> {
    let mut out: Vec<&TaskSummary> = tasks.iter().filter(|t| matches(t, query)).collect();
    out.sort_by(|a, b| {
        b.task
            .created_at
            .cmp(&a.task.created_at)
            .then_with(|| a.task.id.cmp(&b.task.id))
    });
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chocofactory_core::models::{ReviewRef, Task};
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    pub(crate) fn t(id: &str, title: &str, pr: Option<u64>, created_min: u32) -> TaskSummary {
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
            reviews: pr
                .map(|number| ReviewRef {
                    stage: "open_pr".into(),
                    number,
                    url: format!("https://x/pull/{number}"),
                    label: format!("#{number}"),
                })
                .into_iter()
                .collect(),
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
    fn review_number_is_exact_with_and_without_a_sigil() {
        let f = fixture();
        for query in ["289", "#289", "!289", " #289 ", " !289 "] {
            assert_eq!(ids(&search(&f, query)), ["aaaa1111"], "{query:?}");
        }
    }

    #[test]
    fn digits_match_id_prefix_but_sigil_digits_do_not() {
        let f = fixture();
        for bare in ["12", " 12 "] {
            assert_eq!(ids(&search(&f, bare)), ["12ab0000"], "{bare:?}");
        }
        for query in ["#12", "!12"] {
            assert!(search(&f, query).is_empty(), "{query:?}");
        }
    }

    /// Review numbers 42, 420 and 7 whose labels and URLs say otherwise,
    /// ids that begin with digits, and titles that spell the queries out.
    pub(crate) fn mixed() -> Vec<TaskSummary> {
        let reviewed = |id: &str, title: &str, number: u64, label: &str, min: u32| {
            let mut task = t(id, title, Some(number), min);
            task.reviews[0].label = label.to_string();
            task
        };
        let mut seven = reviewed("dddd4444", "Another", 7, "42", 6);
        seven.reviews[0].url = "https://review.example/-/merge_requests/42".into();
        vec![
            reviewed("aaaa1111", "Fix the parser", 42, "MR-alpha", 1),
            reviewed("bbbb2222", "Other", 420, "!42", 2),
            t("42ab0000", "Plain", None, 3),
            t("cccc3333", "Follow up 42, #42 and !42", None, 4),
            t("042f0000", "Ticket 042, #042 and !042", None, 5),
            seven,
        ]
    }

    #[test]
    fn a_review_number_matches_only_its_own_review_whatever_the_label_says() {
        let f = mixed();
        assert_eq!(ids(&search(&f, "42")), ["42ab0000", "aaaa1111"]);
        for query in ["#42", "!42", "  !42  "] {
            assert_eq!(ids(&search(&f, query)), ["aaaa1111"], "{query:?}");
        }
        // Digits are compared as typed.
        assert_eq!(ids(&search(&f, "042")), ["042f0000"]);
        for query in ["#042", "!042"] {
            assert!(search(&f, query).is_empty(), "{query:?}");
        }
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
    fn lone_sigils_and_sigil_text_follow_the_text_rule() {
        let f = vec![
            t("a1", "Fix #abc handling", Some(5), 1),
            t("b1", "C# support", Some(6), 2),
            t("c1", "plain", Some(7), 3),
            t("d1", "Ship it! !abc", None, 4),
        ];
        assert_eq!(ids(&search(&f, "#")), ["b1", "a1"]);
        assert_eq!(ids(&search(&f, "#abc")), ["a1"]);
        assert_eq!(ids(&search(&f, "!")), ["d1"]);
        assert_eq!(ids(&search(&f, "!ABC")), ["d1"]);
    }
}
