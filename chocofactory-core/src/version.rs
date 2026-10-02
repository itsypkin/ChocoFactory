//! Build identity, shared by the daemon and the CLI.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Stamped by a release job via `CHOCO_BUILD_COMMIT`; `None` otherwise.
pub const BUILD_COMMIT: Option<&str> = option_env!("CHOCO_BUILD_COMMIT");

/// "0.1.0 (abc1234)", or "0.1.0 (dev build)" when no commit was stamped.
pub fn long_version() -> String {
    format!("{VERSION} ({})", BUILD_COMMIT.unwrap_or("dev build"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_version_is_dev_build_in_tests() {
        assert_eq!(long_version(), format!("{VERSION} (dev build)"));
    }
}
