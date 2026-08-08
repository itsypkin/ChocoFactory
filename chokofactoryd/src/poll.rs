//! Decides which outcome a `poll` stage's output selects (design §5.2,
//! P2-2).
//!
//! Its own module rather than a private helper in `engine.rs` for the same
//! reason `shell.rs` is: it knows nothing about tasks, stages or the
//! database, so it can be tested against strings alone. `engine.rs` owns
//! the loop and the transitions; this owns the matching.

use regex::Regex;

use crate::workflow_def::PollOutcome;

/// A stage's `outcomes:` list with every pattern compiled once.
///
/// Compiled on stage entry rather than per attempt: a poll runs its command
/// many times over, and `Regex::new` is the expensive half of a match. It
/// also means a bad pattern surfaces as an error on the transition that
/// entered the stage, where a caller can still see it, rather than from
/// inside a detached runner.
#[derive(Debug)]
pub struct CompiledOutcomes(Vec<CompiledOutcome>);

#[derive(Debug)]
struct CompiledOutcome {
    regex: Regex,
    then: String,
}

/// The outcome a poll's output selected, and the rule that selected it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollMatch<'a> {
    /// Outcome name to look up in the stage's `on:` map.
    pub then: &'a str,
    /// The pattern that fired, for the timeline note — an operator reading
    /// "matched" wants to know *which* rule did it, especially when
    /// several could have.
    pub pattern: &'a str,
}

/// Compiles a stage's `outcomes:` in declaration order.
///
/// `WorkflowDefinition::validate` already rejects a definition whose
/// patterns don't compile, so this failing means the definition was built
/// by hand (`StageKind` is a `pub` enum with no private-construction
/// guard) rather than loaded — the same reason `enter_agent_turn` keeps
/// its unknown-role check as a reported error instead of an `expect`.
pub fn compile(outcomes: &[PollOutcome]) -> Result<CompiledOutcomes, regex::Error> {
    outcomes
        .iter()
        .map(|outcome| {
            Ok(CompiledOutcome {
                regex: Regex::new(&outcome.pattern)?,
                then: outcome.then.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(CompiledOutcomes)
}

impl CompiledOutcomes {
    /// The first outcome whose pattern matches `stdout`, or `None` if none
    /// do — which is what keeps the loop polling.
    ///
    /// Ordered, first match wins: §5.1's own example pairs `^SUCCESS$`
    /// with `FAILURE|ERROR`, and a workflow author orders those
    /// deliberately. Patterns are otherwise unanchored, so a bare
    /// `APPROVED` behaves as the substring match §5.2 describes.
    ///
    /// **Surrounding whitespace is trimmed before matching.** This crate's
    /// regex engine anchors `$` to the end of the whole haystack, not to a
    /// line ending, and every command a workflow would poll prints a
    /// trailing newline — so without the trim, §5.1's own `^SUCCESS$`
    /// against `gh pr checks … | sort -u` could never match, which is a
    /// trap that would only show up as a poll that silently runs to its
    /// timeout. Trimming makes the anchored form mean "the output is
    /// exactly this", which is what that example is reaching for; a
    /// pattern wanting per-line anchors can still say `(?m)`.
    ///
    /// Matched against stdout only. stderr is where a polled command puts
    /// its complaints — `gh`'s rate-limit warnings, a git advisory — and
    /// letting those select an outcome would make polls fire on noise.
    pub fn matching<'a>(&'a self, stdout: &str) -> Option<PollMatch<'a>> {
        let haystack = stdout.trim();
        self.0
            .iter()
            .find(|outcome| outcome.regex.is_match(haystack))
            .map(|outcome| PollMatch {
                then: outcome.then.as_str(),
                pattern: outcome.regex.as_str(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(pattern: &str, then: &str) -> PollOutcome {
        PollOutcome {
            pattern: pattern.to_string(),
            then: then.to_string(),
        }
    }

    fn then_of(matched: Option<PollMatch<'_>>) -> Option<&str> {
        matched.map(|m| m.then)
    }

    #[test]
    fn no_outcome_matches_when_the_output_says_nothing_interesting() {
        let compiled = compile(&[outcome("SUCCESS", "green")]).unwrap();
        assert_eq!(then_of(compiled.matching("PENDING")), None);
    }

    #[test]
    fn an_empty_outcomes_list_never_matches() {
        let compiled = compile(&[]).unwrap();
        assert_eq!(then_of(compiled.matching("anything at all")), None);
    }

    #[test]
    fn the_first_matching_outcome_wins_even_when_a_later_one_also_matches() {
        let compiled = compile(&[
            outcome("FAILURE|ERROR", "red"),
            outcome("FAILURE", "also_red"),
        ])
        .unwrap();
        assert_eq!(then_of(compiled.matching("FAILURE")), Some("red"));
    }

    /// Declaration order decides, not the order the patterns happen to
    /// match the string in — the reverse declaration must give the reverse
    /// answer for the same input.
    #[test]
    fn reversing_the_declaration_order_reverses_which_outcome_wins() {
        let compiled = compile(&[
            outcome("FAILURE", "also_red"),
            outcome("FAILURE|ERROR", "red"),
        ])
        .unwrap();
        assert_eq!(then_of(compiled.matching("FAILURE")), Some("also_red"));
    }

    #[test]
    fn a_bare_word_pattern_matches_as_a_substring() {
        let compiled = compile(&[outcome("APPROVED", "approved")]).unwrap();
        assert_eq!(
            then_of(compiled.matching("reviewDecision: APPROVED\n")),
            Some("approved")
        );
    }

    #[test]
    fn an_anchored_pattern_still_rejects_output_it_does_not_describe() {
        let compiled = compile(&[outcome("^SUCCESS$", "green")]).unwrap();
        assert_eq!(then_of(compiled.matching("NOT SUCCESS HERE")), None);
    }

    /// The load-bearing one: §5.1's example is `^SUCCESS$` against the
    /// output of `gh pr checks … | sort -u`, which ends in a newline. This
    /// crate's `$` anchors to the end of the haystack rather than to a line
    /// ending, so without the trim in `matching` this poll would never fire
    /// and the stage would silently run to its timeout.
    #[test]
    fn an_anchored_pattern_matches_output_with_a_trailing_newline() {
        let compiled = compile(&[outcome("^SUCCESS$", "green")]).unwrap();
        assert_eq!(then_of(compiled.matching("SUCCESS\n")), Some("green"));
    }

    /// Trimming must not turn `^…$` into "contains a line like this" —
    /// `sort -u` printing two distinct states means the checks did *not*
    /// all pass, and the design's example depends on that not matching.
    #[test]
    fn an_anchored_pattern_rejects_multi_line_output_with_other_lines() {
        let compiled = compile(&[outcome("^SUCCESS$", "green")]).unwrap();
        assert_eq!(then_of(compiled.matching("PENDING\nSUCCESS\n")), None);
    }

    /// …but a pattern that explicitly asks for per-line anchors gets them.
    #[test]
    fn a_multi_line_flag_opts_in_to_per_line_anchors() {
        let compiled = compile(&[outcome("(?m)^SUCCESS$", "green")]).unwrap();
        assert_eq!(
            then_of(compiled.matching("PENDING\nSUCCESS\n")),
            Some("green")
        );
    }

    #[test]
    fn the_matched_pattern_is_reported_alongside_the_outcome() {
        let compiled = compile(&[
            outcome("^SUCCESS$", "green"),
            outcome("FAILURE|ERROR", "red"),
        ])
        .unwrap();
        let matched = compiled.matching("ERROR").expect("expected a match");
        assert_eq!(matched.then, "red");
        assert_eq!(matched.pattern, "FAILURE|ERROR");
    }

    #[test]
    fn an_uncompilable_pattern_is_an_error_rather_than_a_panic() {
        assert!(compile(&[outcome("(unclosed", "green")]).is_err());
    }
}
