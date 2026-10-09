//! Duration spelling shared by the workflow YAML loader and the `choco` CLI.

use std::time::Duration;

/// Parses durations in the `<integer><unit>` shape used by §5.1's examples
/// (`30s`, `5m`, `1h`) — deliberately not pulling in a duration-parsing
/// crate for a three-suffix format this small.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let mut chars = s.chars();
    let unit = chars.next_back().ok_or_else(|| s.to_string())?;
    let digits = chars.as_str();
    let amount: u64 = digits.parse().map_err(|_| s.to_string())?;
    let multiplier: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return Err(s.to_string()),
    };
    let secs = amount
        .checked_mul(multiplier)
        .ok_or_else(|| s.to_string())?;
    // Zero is never what anyone meant: as a shell `timeout:` it elapses on
    // the first poll, so the command is killed before it can do anything;
    // as a poll `interval:` it's a busy loop. Rejecting it at load time
    // beats either behaviour at runtime.
    if secs == 0 {
        return Err(s.to_string());
    }
    Ok(Duration::from_secs(secs))
}

/// Formats a duration as its non-zero hour, minute and second parts, in that
/// order and unpadded: `102h`, `1h30m`, `1m30s`, `45s`; zero is `0s`. Sub-second
/// parts are dropped. For a whole-unit value it inverts `parse_duration`.
pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 || out.is_empty() {
        out.push_str(&format!("{s}s"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_non_zero_parts() {
        for (secs, want) in [
            (102 * 3600, "102h"),
            (5400, "1h30m"),
            (1800, "30m"),
            (90, "1m30s"),
            (45, "45s"),
            (0, "0s"),
        ] {
            assert_eq!(format_duration(Duration::from_secs(secs)), want);
        }
    }

    #[test]
    fn accepts_s_m_h() {
        assert_eq!(parse_duration("5s"), Ok(Duration::from_secs(5)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            "0s",
            "5",
            "1d",
            "",
            "s",
            "-5s",
            "5 s",
            "99999999999999999999s",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
        // Parses as u64 but overflows when multiplied by 3600.
        assert!(parse_duration("18446744073709551615h").is_err());
    }
}
