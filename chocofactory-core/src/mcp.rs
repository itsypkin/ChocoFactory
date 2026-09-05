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
