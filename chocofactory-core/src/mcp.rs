//! Constants shared by `choco mcp-serve` (the tool's own name) and
//! `chocofactoryd` (which has to recognise the tool's calls back off the
//! event timeline) so the two processes can never disagree on what they're
//! named — issue #73.

use serde_json::{Value, json};

/// The MCP server key `chocofactoryd` writes into `--mcp-config`'s
/// `mcpServers` object.
pub const MCP_SERVER_NAME: &str = "chocofactory";

/// The tool's own, unqualified name.
pub const REPORT_OUTCOME_TOOL_NAME: &str = "report_outcome";

/// The name a model-visible tool call actually carries: `claude` namespaces
/// every MCP tool as `mcp__<server-key>__<tool-name>`, so this is what shows
/// up as a `tool_call` event's `tool` field on the timeline.
pub fn qualified_report_outcome_tool_name() -> String {
    format!("mcp__{MCP_SERVER_NAME}__{REPORT_OUTCOME_TOOL_NAME}")
}

/// A report heading — or a required section's name — reduced to what
/// matching should care about (issue #95).
///
/// Shared rather than written twice, because the two copies would be a
/// workflow loader deciding that two section names are different and a tool
/// deciding they're the same: a stage could then require a section its own
/// report can never satisfy, rejecting every review, forever, with nothing
/// the model could do about it. Review of #95 caught exactly that pair
/// (`Branches → tests` and `Branches -> tests`).
///
/// Deliberately forgiving in both directions. The rule exists so the walks
/// get written down; spending a retry on markdown would be a pure loss:
///
/// - `→` and `->` are the same arrow.
/// - Leading `#`, `>`, backticks, quotes and paired emphasis markers go.
/// - A leading ordered-list marker (`1.`, `2)`) goes, so a model that
///   numbers the sections it was handed as an ordered list still matches.
/// - A leading bullet (`- `, `* `, `+ `) goes, like any other decoration:
///   `choco`'s matcher holds a bulleted heading to the same rule as a
///   `##` one, having tried a stricter rule and withdrawn it.
/// - A hyphen *between* word characters becomes a space, so `Side-effects`
///   matches `Side effects` — but `->` survives, because its `-` is not
///   between two word characters.
/// - Runs of whitespace collapse, and the whole thing lowercases.
pub fn normalize_report_heading(text: &str) -> String {
    let lowered = text.replace('→', "->").to_lowercase();
    let without_markers = strip_leading_markers(&lowered);
    // Closing decoration too, so `**Findings**` ends up bare rather than
    // with a `**` the caller would read as content after the name.
    let without_markers = without_markers.trim_end_matches(['*', '_', '#', '`', '"', ' ', '\t']);
    let unhyphenated = split_word_internal_hyphens(without_markers);
    unhyphenated
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `text` is a bullet — `- `, `* ` or `+ ` at the front, before any
/// other decoration.
///
/// Only [`strip_leading_markers`] needs this, to take the marker off
/// without mistaking the `-` of `-> tests` for one. Nothing outside this
/// module distinguishes a bulleted heading from any other kind.
fn starts_a_bullet(text: &str) -> bool {
    let trimmed = text.trim_start();
    let mut chars = trimmed.chars();
    matches!(chars.next(), Some('-' | '*' | '+')) && chars.next().is_some_and(char::is_whitespace)
}

/// Markdown decoration and list numbering at the front of a line.
fn strip_leading_markers(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        let before = rest;
        // `*` is here as well as in the `**` case below: round 2 of #95's
        // review found `*Findings*` matching nothing while `_Findings_`
        // worked, which is arbitrary from the model's side. Safe because
        // bullet detection reads the *raw* line, not this one.
        rest = rest.trim_start_matches(['#', '>', '`', '"', '_', '*']);
        // `**bold**` is emphasis, and a lone `*`/`-`/`+` before a space is
        // a bullet; both are decoration around the heading text.
        while let Some(stripped) = rest.strip_prefix("**") {
            rest = stripped;
        }
        if starts_a_bullet(rest) {
            // Sliced from the marker, not from byte 0: `rest` may still
            // carry leading whitespace (`## - Findings`), where cutting
            // one byte off the front removes the space rather than the
            // bullet. The loop recovered on its next pass, by accident.
            rest = &rest.trim_start()[1..];
        }
        rest = strip_ordered_list_marker(rest);
        rest = rest.trim_start();
        if rest == before {
            return rest;
        }
    }
}

/// `1. `, `2) ` and friends — but not `1.5`, which is a number in prose.
fn strip_ordered_list_marker(text: &str) -> &str {
    let digits: usize = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return text;
    }
    let rest = &text[digits..];
    let Some(rest) = rest.strip_prefix(['.', ')']) else {
        return text;
    };
    if rest.starts_with(char::is_whitespace) {
        rest
    } else {
        text
    }
}

fn split_word_internal_hyphens(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars
        .iter()
        .enumerate()
        .map(|(index, &ch)| {
            let word_before = index > 0 && chars[index - 1].is_alphanumeric();
            let word_after = chars
                .get(index + 1)
                .is_some_and(|next| next.is_alphanumeric());
            if ch == '-' && word_before && word_after {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

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
pub const MAX_SECTION_REJECTIONS: u32 = 2;

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

/// The required sections `summary` doesn't account for, in the order the
/// stage declared them. A section is accounted for when the summary has a
/// heading line for it *and* something under it.
///
/// Empty when `required` is empty, which is every stage that doesn't ask
/// for sections — i.e. all of them before #95, and every workflow that
/// never opts in.
pub fn missing_sections(summary: &str, required: &[String]) -> Vec<String> {
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
    // is only the start of a longer word. Redundant with the punctuation
    // rule below as that rule stands today, and kept anyway: it fires
    // *before* whatever clause a later change adds, and a later clause
    // silently dropping this guard is what round 3 of #95's own review
    // found. Delete either one and the hole comes back.
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
pub fn missing_sections_message(missing: &[String], required: &[String]) -> String {
    format!(
        "Your report's 'summary' is missing {}: each required section needs a heading line with \
         something under it (a section with genuinely nothing in it says \"<section>: none\"). Required sections, in \
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

/// The tool as the model sees it.
///
/// Both the description and the schema are generated from `outcomes`, which is
/// the whole reason no workflow author ever writes about this tool in a prompt
/// file: there is no second copy of the stage's `on:` keys to fall out of sync.
pub fn tool_definition(stage: &StageReport) -> Value {
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
                 under it — a section with genuinely nothing in it says \"<section>: none\": {}. Write in that \
                 order. A report missing any of them is rejected and you will be asked to call \
                 again.",
                stage.required_sections.join(", "),
            )
        },
    );

    json!({
        "name": REPORT_OUTCOME_TOOL_NAME,
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

/// The result of checking one `report_outcome` call: the text the model
/// sees and whether it is a rejection it can retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCheck {
    pub text: String,
    pub is_error: bool,
}

impl ReportCheck {
    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }
}

/// Validates one `report_outcome` call's arguments against the stage. The one
/// place the rules live: `choco mcp-serve` (claude) and the omp adapter's
/// host tool both call it, so they cannot disagree.
///
/// `thin_reports` counts section-missing rejections for the calling process.
pub fn check_report_call(
    stage: &StageReport,
    thin_reports: &mut u32,
    arguments: &Value,
) -> ReportCheck {
    let outcomes = &stage.outcomes[..];
    let outcome = match arguments.get("outcome").and_then(Value::as_str) {
        Some(outcome) if !outcome.trim().is_empty() => outcome.trim(),
        _ => {
            return ReportCheck::error(format!(
                "'outcome' is required and must be a non-empty string.{}",
                allowed_clause(outcomes)
            ));
        }
    };

    if !outcomes.is_empty() && !outcomes.iter().any(|allowed| allowed == outcome) {
        return ReportCheck::error(format!(
            "'{outcome}' is not a valid outcome for this stage.{} Call this tool again with one \
             of those.",
            allowed_clause(outcomes)
        ));
    }

    // `summary` is documented as required (it may be empty); the schema's
    // `required` array is only a hint to the model, so enforce it here.
    let Some(summary) = arguments.get("summary").and_then(Value::as_str) else {
        return ReportCheck::error("'summary' is required and must be a string (it may be empty).");
    };

    // Checked after the outcome so a call wrong in both ways is told about
    // the cheaper problem first.
    let missing = missing_sections(summary, &stage.required_sections);
    if !missing.is_empty() {
        // Counted before the branch, and never saturated, so the number in
        // the message is the attempt this actually is.
        *thin_reports += 1;
        if *thin_reports <= MAX_SECTION_REJECTIONS {
            return ReportCheck::error(missing_sections_message(
                &missing,
                &stage.required_sections,
            ));
        }
        return ReportCheck::success(format!(
            "Recorded outcome '{outcome}'. Its report is still missing {}, and this is attempt \
             {}, so it was recorded as it stands.",
            quoted_list(&missing),
            *thin_reports,
        ));
    }

    ReportCheck::success(format!("Recorded outcome '{outcome}'."))
}

#[cfg(test)]
mod tests {
    use super::normalize_report_heading as normalize;

    #[test]
    fn markdown_decoration_does_not_change_a_heading() {
        for line in [
            "Findings",
            "## Findings",
            "**Findings**",
            "*Findings*",
            "  ###   findings  ",
            "> `Findings`",
        ] {
            assert_eq!(normalize(line), "findings", "line {line:?}");
        }
    }

    /// The required sections are handed to the model as an ordered list, so
    /// a model numbering its headings is an obvious formatting choice — and
    /// before #95's review it defeated the matcher completely.
    #[test]
    fn ordered_list_numbering_is_not_part_of_the_heading() {
        assert_eq!(normalize("### 1. Reviewed"), "reviewed");
        assert_eq!(normalize("2) Predictions"), "predictions");
    }

    /// A number that isn't a list marker stays, or "1.5x slower" would
    /// become a heading candidate for whatever follows it.
    #[test]
    fn a_number_in_prose_is_left_alone() {
        assert_eq!(normalize("1.5 seconds"), "1.5 seconds");
        assert_eq!(normalize("12 findings"), "12 findings");
    }

    /// Both spellings of the arrow, and a hyphenated compound, reach the
    /// same key — `Branches -> tests` is the workflow author's spelling and
    /// `Branches → tests` is the prompt's.
    #[test]
    fn arrows_and_word_hyphens_normalize_together() {
        assert_eq!(normalize("Branches → tests"), "branches -> tests");
        assert_eq!(normalize("branches  ->  TESTS"), "branches -> tests");
        assert_eq!(normalize("## Side-effects"), "side effects");
    }

    /// The marker is decoration like any other.
    #[test]
    fn a_bullet_marker_is_decoration() {
        assert_eq!(normalize("- Findings"), "findings");
        assert_eq!(normalize("* Findings"), "findings");
        assert_eq!(
            normalize("- Side effects of the retry"),
            "side effects of the retry"
        );
    }

    #[test]
    fn a_bullet_is_recognised_only_with_a_following_space() {
        assert!(super::starts_a_bullet("- Findings"));
        assert!(super::starts_a_bullet("  * Findings"));
        assert!(!super::starts_a_bullet("-> tests"));
        assert!(!super::starts_a_bullet("## Findings"));
    }
}
