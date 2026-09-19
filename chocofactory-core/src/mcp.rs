//! Constants shared by `choco mcp-serve` (the tool's own name) and
//! `chocofactoryd` (which has to recognise the tool's calls back off the
//! event timeline) so the two processes can never disagree on what they're
//! named — issue #73.

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
/// - A leading bullet (`- `, `* `, `+ `) goes. Whether a bullet may head a
///   section at all is `choco`'s decision, not this function's — see
///   `mcp::heading_for` there, which asks [`starts_a_bullet`] separately.
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
/// [`normalize_report_heading`] strips the marker like any other, so this
/// is how a caller that cares can still tell. `choco`'s tool server does:
/// a bullet inside a findings list is much more often a finding that
/// happens to start with a section's name ("- Side effects of the retry
/// are untested") than the heading of a new section, so it only accepts a
/// bullet as a heading when the name is the whole of it.
pub fn starts_a_bullet(text: &str) -> bool {
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

    /// The marker itself is decoration like any other; whether a bullet
    /// may *head* a section is the caller's call, via `starts_a_bullet`.
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
